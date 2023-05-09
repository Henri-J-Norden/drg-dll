#!/bin/sh

DIR=sdk/src


# Prepare source
#rm -r $DIR
rm -r sdk/
git checkout HEAD -- sdk/
mkdir $DIR
echo "" > $DIR/lib.rs

# Build sdk_gen.dll
cargo clean
cargo build -p sdk_gen

# Inject
start steam://rungameid/548430
sleep 5
./Injector.exe -n FSD-Win64-Shipping.exe -i target/debug/sdk_gen.dll
