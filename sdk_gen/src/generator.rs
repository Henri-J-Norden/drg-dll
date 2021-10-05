use crate::buf_writer::BufWriter;
use crate::game::{
    self, EPropertyFlags, FBoolProperty, FProperty, PropertyDisplayable, TPair, UEnum,
};
use crate::{sdk_file, sdk_path};

use common::win::file::{self, File};
use common::List;
use common::SplitIterator;
use common::{
    EClassCastFlags, FName, GUObjectArray, UClass, UFunction, UObject, UPackage, UStruct,
};

use core::cell::Cell;
use core::cmp::Ordering;
use core::fmt::{self, Display, Formatter, Write};
use core::str;

#[derive(macros::NoPanicErrorDebug)]
pub enum Error {
    Game(#[from] game::Error),
    File(#[from] file::Error),
    Fmt(#[from] fmt::Error),

    ZeroSizedField,
    BadBitfieldSize(u8),
    LastBitfield,
    MaxPackages,
    MaxBitfields,
    BitfieldFull,

    MaxParameters,
}

struct Package {
    ptr: *mut UPackage,
    file: File,
}

impl Drop for Package {
    fn drop(&mut self) {
        unsafe {
            (*self.ptr).PIEInstanceID = -1;
        }
    }
}

pub struct Generator {
    lib_rs: File,
    packages: List<Package, 160>,
    blueprint_generated_package_file: BufWriter<File>,
}

impl Generator {
    pub unsafe fn new() -> Result<Generator, Error> {
        let mut lib_rs = File::new(sdk_file!("src/lib.rs"))?;
        lib_rs.write_str(
            "\
            #![no_std]\n\
            #![allow(dead_code, non_camel_case_types, non_snake_case, non_upper_case_globals)]\n\
            #![allow(clippy::missing_safety_doc, clippy::too_many_arguments, clippy::type_complexity)]\n\
            pub mod blueprint_generated;\n",
        )?;

        Ok(Generator {
            lib_rs,
            packages: List::new(),
            blueprint_generated_package_file: BufWriter::new(File::new(sdk_file!(
                "src/blueprint_generated.rs"
            ))?),
        })
    }

    pub unsafe fn generate_sdk(&mut self) -> Result<(), Error> {
        for object in (*GUObjectArray).iter().filter(|o| !o.is_null()) {
            if (*object).fast_is(
                EClassCastFlags::CASTCLASS_UClass | EClassCastFlags::CASTCLASS_UScriptStruct,
            ) {
                self.generate_structure(object.cast())?;
            } else if (*object).fast_is(EClassCastFlags::CASTCLASS_UEnum) {
                self.generate_enum(object.cast())?;
            }
        }
        Ok(())
    }

    unsafe fn get_package(&mut self, object: *mut UObject) -> Result<&mut Package, Error> {
        let package = (*object).package_mut();
        let is_unseen_package = (*package).PIEInstanceID == -1;

        if is_unseen_package {
            self.register_package(package)?;
        }

        let package = (*package).PIEInstanceID as usize;
        Ok(self.packages.get_unchecked_mut(package))
    }

    unsafe fn get_package_file(
        &mut self,
        object: *mut UObject,
    ) -> Result<BufWriter<&mut File>, Error> {
        Ok(BufWriter::new(&mut self.get_package(object)?.file))
    }

    unsafe fn register_package(&mut self, package: *mut UPackage) -> Result<(), Error> {
        let package_name = (*package).short_name();

        // Create a Rust module file for this package.
        let file = {
            let mut path = List::<u8, 260>::new();
            write!(
                &mut path,
                concat!(sdk_path!(), "/src/{}.rs\0"),
                package_name
            )?;
            File::new(path)?
        };

        // Declare the module in the SDK lib.rs.
        writeln!(&mut self.lib_rs, "pub mod {};", package_name)?;

        // Register this package's index in our package cache.
        (*package).PIEInstanceID = self.packages.len() as i32;

        let p = Package { ptr: package, file };

        // Save the package to our cache.
        self.packages.push(p).map_err(|_| Error::MaxPackages)?;

        Ok(())
    }

    unsafe fn generate_enum(&mut self, enumeration: *mut UEnum) -> Result<(), Error> {
        let variants = (*enumeration).Names.as_slice();

        let (last, rest) = if let Some(v) = variants.split_last() {
            v
        } else {
            // Don't generate empty enums.
            return Ok(());
        };

        let is_last_variant_autogenerated_max = {
            let last = last.Key.text();
            last.ends_with("_MAX") || last.ends_with("_Max")
        };

        let representation = if is_last_variant_autogenerated_max {
            get_enum_representation(rest)
        } else {
            get_enum_representation(variants)
        };

        let mut file = self.get_package_file(enumeration.cast())?;

        writeln!(
            file,
            "// {}\n#[repr(transparent)]\npub struct {name}({});\n\nimpl {name} {{",
            *enumeration,
            representation,
            name = (*enumeration).name(),
        )?;

        for variant in rest.iter() {
            write_enum_variant(&mut file, variant)?;
        }

        if !is_last_variant_autogenerated_max {
            write_enum_variant(&mut file, last)?;
        }

        writeln!(file, "}}\n")?;

        Ok(())
    }

    unsafe fn generate_structure(&mut self, structure: *mut UStruct) -> Result<(), Error> {
        if (*structure).fast_is(EClassCastFlags::CASTCLASS_UClass) {
            let class = structure.cast::<UClass>();

            if (*class).is_blueprint_generated() {
                return StructGenerator::new(
                    structure,
                    (*class).package(),
                    &mut self.blueprint_generated_package_file,
                    true,
                )
                .generate();
            }
        }

        let package = self.get_package(structure.cast())?;

        // TODO(perf): Don't need to create a new `BufWriter` if the previous object is from the same package.
        // Reuse previous buffer to reduce total `WriteFile` calls.
        let file = BufWriter::new(&mut package.file);

        StructGenerator::new(structure, package.ptr, file, false).generate()
    }
}

unsafe fn get_enum_representation(variants: &[TPair<FName, i64>]) -> &'static str {
    let max_discriminant_value = variants.iter().map(|v| v.Value).max().unwrap_or(0);

    if max_discriminant_value <= u8::MAX.into() {
        "u8"
    } else if max_discriminant_value <= u32::MAX.into() {
        "u32"
    } else {
        "u64"
    }
}

unsafe fn write_enum_variant(
    mut out: impl Write,
    variant: &TPair<FName, i64>,
) -> Result<(), Error> {
    let mut text = variant.Key.text();

    if let Some(text_stripped) = text
        .bytes()
        .rposition(|c| c == b':')
        .and_then(|i| text.get(i + 1..))
    {
        text = text_stripped;
    }

    if text == "Self" {
        // `Self` is a Rust keyword.
        text = "SelfVariant";
    }

    if variant.Key.number() > 0 {
        writeln!(
            out,
            "    pub const {}_{}: Self = Self({});",
            text,
            variant.Key.number() - 1,
            variant.Value,
        )?;
    } else {
        writeln!(
            out,
            "    pub const {}: Self = Self({});",
            text, variant.Value,
        )?;
    }

    Ok(())
}

struct StructGenerator<W: Write> {
    structure: *mut UStruct,
    package: *const UPackage,
    out: W,
    offset: i32,
    bitfields: List<List<*const FBoolProperty, 64>, 64>,
    last_bitfield_offset: Option<i32>,
    is_blueprint_generated: bool,
    inherited_type: List<u8, 128>,
}

impl<W: Write> StructGenerator<W> {
    pub fn new(
        structure: *mut UStruct,
        package: *const UPackage,
        out: W,
        is_blueprint_generated: bool,
    ) -> StructGenerator<W> {
        StructGenerator {
            structure,
            package,
            out,
            offset: 0,
            bitfields: List::new(),
            last_bitfield_offset: None,
            is_blueprint_generated,
            inherited_type: List::new(),
        }
    }

    pub unsafe fn generate(&mut self) -> Result<(), Error> {
        if (*self.structure).PropertiesSize == 0 {
            return Ok(());
        }

        self.write_header()?;
        self.add_fields()?;
        writeln!(self.out, "}}\n")?;

        if !self.bitfields.is_empty() {
            self.add_bitfield_getters_and_setters()?;
        }

        self.add_deref_impls()?;

        self.add_functions()?;

        Ok(())
    }

    unsafe fn write_header(&mut self) -> Result<(), Error> {
        let base = (*self.structure).SuperStruct;

        if base.is_null() {
            writeln!(
                self.out,
                "// {} is {} bytes.\n#[repr(C, align({}))]\npub struct {} {{",
                *self.structure,
                (*self.structure).PropertiesSize,
                (*self.structure).MinAlignment,
                (*self.structure).name()
            )?;
        } else {
            self.write_header_inherited(base)?;
        }

        Ok(())
    }

    unsafe fn write_header_inherited(&mut self, base: *mut UStruct) -> Result<(), Error> {
        self.offset = (*base).PropertiesSize;

        writeln!(
            self.out,
            "// {}: {} is {} bytes ({} inherited).\n#[repr(C, align({}))]\npub struct {} {{",
            self.structure as usize,
            *self.structure,
            (*self.structure).PropertiesSize,
            self.offset,
            (*self.structure).MinAlignment,
            (*self.structure).name()
        )?;

        let base_name = (*base).name();
        let base_package = (*base).package();

        let is_base_blueprint_generated = self.is_blueprint_generated
            && (*base).fast_is(EClassCastFlags::CASTCLASS_UClass)
            && (*base.cast::<UClass>()).is_blueprint_generated();

        if is_base_blueprint_generated || base_package == self.package {
            write!(self.inherited_type, "{}", base_name)?;
            
            writeln!(
                self.out,
                "    // offset: 0, size: {}\n    base: {},\n",
                self.offset, base_name
            )?;
        } else {
            let short_name = (*base_package).short_name();

            write!(self.inherited_type, "crate::{}::{}", short_name, base_name)?;

            writeln!(
                self.out,
                "    // offset: 0, size: {}\n    base: crate::{}::{},\n",
                self.offset,
                short_name,
                base_name
            )?;
        }

        Ok(())
    }

    unsafe fn add_fields(&mut self) -> Result<(), Error> {
        let mut property = (*self.structure).ChildProperties.cast::<FProperty>();

        while !property.is_null() {
            self.process_property(property)?;
            property = (*property).base.Next.cast();
        }

        self.add_end_of_struct_padding_if_needed()?;

        Ok(())
    }

    unsafe fn process_property(&mut self, property: *const FProperty) -> Result<(), Error> {
        let size = (*property).ElementSize * (*property).ArrayDim;

        if size == 0 {
            return Err(Error::ZeroSizedField);
        }

        if (*property).is(EClassCastFlags::CASTCLASS_FBoolProperty) && (*property.cast::<FBoolProperty>()).is_bitfield() {
            self.process_bool_property(property.cast())?;
        } else {
            self.add_padding_if_needed(property)?;

            if self.is_blueprint_generated {
                self.process_blueprint_property(property, size)?;
            } else {
                writeln!(
                    self.out,
                    "    // offset: {offset}, size: {size}\n    pub {name}: {typ},\n",
                    offset = self.offset,
                    size = size,
                    name = (*property).base.NamePrivate,
                    typ = PropertyDisplayable::new(
                        property,
                        self.package,
                        self.is_blueprint_generated
                    ),
                )?;
            }

            self.offset += size;
        }

        Ok(())
    }

    unsafe fn process_bool_property(
        &mut self,
        property: *const FBoolProperty,
    ) -> Result<(), Error> {
        let offset = (*property).base.Offset;

        if self.last_bitfield_offset.map_or(false, |o| offset == o) {
            self.bitfields
                .last_mut()
                .ok_or(Error::LastBitfield)?
                .push(property)
                .map_err(|_| Error::BitfieldFull)?;
        } else {
            self.add_padding_if_needed(property.cast())?;

            let size = (*property).FieldSize;

            let representation = if size == 1 {
                "u8"
            } else if size == 2 {
                "u16"
            } else if size == 4 {
                "u32"
            } else if size == 8 {
                "u64"
            } else {
                return Err(Error::BadBitfieldSize(size));
            };

            writeln!(
                self.out,
                "    // offset: {offset}, size: {size}\n    pub bitfield_at_{offset}: {representation},\n",
                offset = offset,
                size = size,
                representation = representation,
            )?;

            self.last_bitfield_offset = Some(offset);

            self.bitfields
                .push({
                    let mut b = List::new();
                    b.push(property).map_err(|_| Error::BitfieldFull)?;
                    b
                })
                .map_err(|_| Error::MaxBitfields)?;

            self.offset += i32::from(size);
        }

        Ok(())
    }

    unsafe fn process_blueprint_property(
        &mut self,
        property: *const FProperty,
        size: i32,
    ) -> Result<(), Error> {
        write!(
            self.out,
            "    // offset: {offset}, size: {size}\n    pub ",
            offset = self.offset,
            size = size,
        )?;

        let name = (*property).base.NamePrivate;
        let cleaned_name = CleanedName::new(name);

        write!(
            self.out,
            "{}: {},",
            cleaned_name,
            PropertyDisplayable::new(property, self.package, self.is_blueprint_generated)
        )?;

        let num_invalid_characters_replaced = cleaned_name.num_invalid_characters_replaced.get();

        if num_invalid_characters_replaced > 1 {
            writeln!(
                self.out,
                "// NOTE: Property's original name is \"{}\". Replaced {} invalid characters.\n",
                name.text(),
                num_invalid_characters_replaced
            )?;
        } else {
            writeln!(self.out, "\n")?;
        }

        Ok(())
    }

    unsafe fn add_pad_field(&mut self, from_offset: i32, to_offset: i32) -> Result<(), Error> {
        writeln!(
            self.out,
            "    // offset: {offset}, size: {size}\n    pad_at_{offset}: [u8; {size}],\n",
            offset = from_offset,
            size = to_offset - from_offset,
        )?;

        self.offset = to_offset;

        Ok(())
    }

    unsafe fn add_padding_if_needed(&mut self, property: *const FProperty) -> Result<(), Error> {
        let offset = (*property).Offset;

        match self.offset.cmp(&offset) {
            Ordering::Less => {
                // We believe the structure is currently at `self.offset`. This
                // property is some bytes ahead at `offset`. So we need to add
                // (offset - self.offset) bytes of padding to reach the
                // property.
                self.add_pad_field(self.offset, offset)?;
            }

            Ordering::Greater => {
                // The property is some bytes behind our reckoning of the
                // current offset. Until we figure out a better way to handle
                // these lagged properties, we should emit a warning so the SDK
                // user has some idea as to why some fields in some structures
                // don't line up with what they're seeing in ReClass.
                writeln!(self.out, "    // WARNING: Property \"{}\" thinks its offset is {}. We think its offset is {}.", (*property).base.NamePrivate, offset, self.offset)?;
            }

            Ordering::Equal => {
                // Nothing to do. Our reckoning off the current offset matches
                // the property's offset. No padding or warning required.
            }
        }

        Ok(())
    }

    unsafe fn add_end_of_struct_padding_if_needed(&mut self) -> Result<(), Error> {
        let struct_size = (*self.structure).PropertiesSize;

        match self.offset.cmp(&struct_size) {
            // See comments in `add_padding_if_needed()` for explanation.
            Ordering::Less => self.add_pad_field(self.offset, struct_size)?,

            Ordering::Greater => writeln!(
                self.out,
                "    // WARNING: This structure thinks its size is {}. We think its size is {}.",
                struct_size, self.offset
            )?,

            Ordering::Equal => {}
        }

        Ok(())
    }

    unsafe fn add_bitfield_getters_and_setters(&mut self) -> Result<(), Error> {
        writeln!(self.out, "impl {} {{", (*self.structure).name())?;

        for bitfield in self.bitfields.iter() {
            for &property in bitfield.iter() {
                let mask = u64::from((*property).ByteMask);
                let offset = (*property).ByteOffset;
                let mask = mask << (8 * offset);
                writeln!(
                    self.out,
                    include_str!("bitfield_getter_setter.fmt"),
                    property_name = (*property).base.base.NamePrivate,
                    offset = (*property).base.Offset,
                    mask = mask,
                )?;
            }
        }

        writeln!(self.out, "}}\n")?;

        Ok(())
    }

    unsafe fn add_deref_impls(&mut self) -> Result<(), Error> {
        if !self.inherited_type.is_empty() {
            writeln!(
                self.out,
                include_str!("deref.fmt"),
                child = (*self.structure).name(),
                parent = str::from_utf8_unchecked(self.inherited_type.as_slice()),
            )?;
        }

        Ok(())
    }

    unsafe fn add_functions(&mut self) -> Result<(), Error> {
        let mut property = (*self.structure).Children;
        let mut has_at_least_one_function = false;

        while !property.is_null() {
            if (*property).fast_is(EClassCastFlags::CASTCLASS_UFunction) {
                if !has_at_least_one_function {
                    has_at_least_one_function = true;
                    writeln!(self.out, "impl {} {{", (*self.structure).name())?;
                }

                self.process_function(property.cast())?;
            }

            property = (*property).Next;
        }

        if has_at_least_one_function {
            writeln!(self.out, "}}\n")?;
        }

        Ok(())
    }

    unsafe fn process_function(&mut self, function: *const UFunction) -> Result<(), Error> {
        enum Kind {
            Input,
            Output,
        }

        struct Parameter {
            property: *const FProperty,
            kind: Kind,
        }

        struct Parameters {
            parameters: List<Parameter, 32>,
            package: *const UPackage,
            is_struct_blueprint_generated: bool,
            num_outputs: u8,
        }

        impl Parameters {
            fn new(package: *const UPackage, is_struct_blueprint_generated: bool) -> Parameters {
                Parameters {
                    parameters: List::new(),
                    package,
                    is_struct_blueprint_generated,
                    num_outputs: 0,
                }
            }

            fn add(&mut self, parameter: Parameter) -> Result<(), Error> {
                self.parameters
                    .push(parameter)
                    .map_err(|_| Error::MaxParameters)?;
                Ok(())
            }

            fn process(&mut self, property: *const FProperty) -> Result<(), Error> {
                let flags = unsafe { (*property).PropertyFlags };

                let kind = if flags.contains(EPropertyFlags::CPF_ReturnParm) || (flags.contains(EPropertyFlags::CPF_OutParm) && !flags.contains(EPropertyFlags::CPF_ConstParm)) {
                    self.num_outputs += 1;
                    Kind::Output
                } else if flags.contains(EPropertyFlags::CPF_Parm) {
                    Kind::Input
                } else {
                    return Ok(());
                };

                self.add(Parameter { property, kind })?;

                Ok(())
            }
        }

        struct Inputs<'a>(&'a Parameters);

        impl<'a> Display for Inputs<'a> {
            fn fmt(&self, f: &mut Formatter) -> Result<(), fmt::Error> {
                for parameter in self.0.parameters.iter() {
                    if let Kind::Input = parameter.kind {
                        let parameter = parameter.property;
                        let name = CleanedName::new(unsafe { (*parameter).base.NamePrivate });
                        let typ = PropertyDisplayable::new(
                            parameter,
                            self.0.package,
                            self.0.is_struct_blueprint_generated,
                        );
                        write!(f, "{}: {}, ", name, typ)?;
                    }
                }

                Ok(())
            }
        }

        struct Outputs<'a>(&'a Parameters);

        impl<'a> Display for Outputs<'a> {
            fn fmt(&self, f: &mut Formatter) -> Result<(), fmt::Error> {
                match self.0.num_outputs {
                    0 => return Ok(()),
                    1 => write!(f, "-> ")?,
                    _ => write!(f, "-> (")?,
                }

                for parameter in self.0.parameters.iter() {
                    if let Kind::Output = parameter.kind {
                        let typ = PropertyDisplayable::new(
                            parameter.property,
                            self.0.package,
                            self.0.is_struct_blueprint_generated,
                        );

                        if self.0.num_outputs == 1 {
                            write!(f, "{} ", typ)?;
                            return Ok(());
                        } else {
                            write!(f, "{}, ", typ)?;
                        }
                    }
                }

                if self.0.num_outputs > 1 {
                    write!(f, ") ")?;
                }

                Ok(())
            }
        }

        struct DeclareStructFields<'a>(&'a Parameters);

        impl<'a> Display for DeclareStructFields<'a> {
            fn fmt(&self, f: &mut Formatter) -> Result<(), fmt::Error> {
                for parameter in self.0.parameters.iter() {
                    let property = parameter.property;
                    let name = CleanedName::new(unsafe { (*property).base.NamePrivate });
                    let typ = PropertyDisplayable::new(
                        property,
                        self.0.package,
                        self.0.is_struct_blueprint_generated,
                    );

                    if let Kind::Input = parameter.kind {
                        write!(f, "\n            {}: {}, ", name, typ)?;
                    } else {
                        write!(
                            f,
                            "\n            {}: core::mem::MaybeUninit<{}>, ",
                            name, typ
                        )?;
                    }
                }

                Ok(())
            }
        }

        struct InitStructFields<'a>(&'a Parameters);

        impl<'a> Display for InitStructFields<'a> {
            fn fmt(&self, f: &mut Formatter) -> Result<(), fmt::Error> {
                for parameter in self.0.parameters.iter() {
                    let name = CleanedName::new(unsafe { (*parameter.property).base.NamePrivate });

                    if let Kind::Input = parameter.kind {
                        write!(f, "\n            {}, ", name)?;
                    } else {
                        write!(
                            f,
                            "\n            {}: core::mem::MaybeUninit::uninit(), ",
                            name
                        )?;
                    }
                }

                Ok(())
            }
        }

        struct ReturnValues<'a>(&'a Parameters);

        impl<'a> Display for ReturnValues<'a> {
            fn fmt(&self, f: &mut Formatter) -> Result<(), fmt::Error> {
                match self.0.num_outputs {
                    0 => return Ok(()),
                    1 => write!(f, "\n        ")?,
                    _ => write!(f, "\n        (")?,
                }

                for parameter in self.0.parameters.iter() {
                    if let Kind::Output = parameter.kind {
                        let name =
                            CleanedName::new(unsafe { (*parameter.property).base.NamePrivate });

                        if self.0.num_outputs == 1 {
                            write!(f, "parameters.{}.assume_init()", name)?;
                            return Ok(());
                        } else {
                            write!(f, "parameters.{}.assume_init(), ", name)?;
                        }
                    }
                }

                if self.0.num_outputs > 1 {
                    write!(f, ")")?;
                }

                Ok(())
            }
        }

        let mut parameters = Parameters::new(self.package, self.is_blueprint_generated);
        let mut property = (*function).ChildProperties.cast::<FProperty>();

        while !property.is_null() {
            parameters.process(property)?;
            property = (*property).base.Next.cast::<FProperty>();
        }

        let cleaned_name = CleanedName::new((*function).NamePrivate);

        writeln!(
            self.out,
            include_str!("function.fmt"),
            name = cleaned_name,
            full_name = *function,
            inputs = Inputs(&parameters),
            outputs = Outputs(&parameters),
            declare_struct_fields = DeclareStructFields(&parameters),
            init_struct_fields = InitStructFields(&parameters),
            return_values = ReturnValues(&parameters),
        )?;

        Ok(())
    }
}

struct CleanedName {
    name: FName,
    num_invalid_characters_replaced: Cell<u8>,
}

impl CleanedName {
    fn new(name: FName) -> CleanedName {
        CleanedName {
            name,
            num_invalid_characters_replaced: Cell::new(0),
        }
    }
}

impl Display for CleanedName {
    fn fmt(&self, f: &mut Formatter) -> Result<(), fmt::Error> {
        let mut num_pieces_added = 0;
        let text = unsafe { self.name.text() };

        if text.starts_with(|c: char| c.is_ascii_digit()) {
            f.write_str("Func_")?;
        }

        for piece in SplitIterator::new(text.as_bytes(), |c| !c.is_ascii_alphanumeric() && c != b'_') {
            if num_pieces_added > 0 {
                f.write_char('_')?;
            }

            write!(f, "{}", unsafe { str::from_utf8_unchecked(piece) })?;

            num_pieces_added += 1;
        }

        let number = self.name.number();

        if number > 0 {
            write!(f, "_{}", number - 1)?;
        }

        self.num_invalid_characters_replaced
            .set(num_pieces_added - 1);

        Ok(())
    }
}
