#!/bin/sh

# Build
cargo build

# Inject
start steam://rungameid/548430
sleep 5
./Injector.exe -n FSD-Win64-Shipping.exe -i target/debug/hook.dll
