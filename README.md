# drg

## Primary goal
Produce a .DLL that players can use to write native Unreal Engine modifications for [Deep Rock Galactic](https://www.deeprockgalactic.com/).

## Secondary goals
Use these restrictions to learn new things:
* No Rust standard library (enforced through `#![no_std]`)
* No third-party crate dependencies
* No heap allocations
* No panic branches (enforced through unlinkable panic_handler)

## Usage
0. <i>[Install Rust](https://www.rust-lang.org/tools/install)</i>
1. Build the sdk_gen package:
```cmd
echo nul > sdk_gen/src/lib.rs
cargo build -p sdk_gen
```
2. Run DRG and inject the built target/debug/sdk_gen.dll to populate sdk/src/
3. Now you can build the workspace with `cargo build`
4. Run DRG and inject target/debug/hook.dll
