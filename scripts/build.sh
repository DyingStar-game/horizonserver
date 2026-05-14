#!/bin/bash

set -e

mkdir -p "Horizon/plugins"
RUSTFLAGS="" cargo build --locked --release
cp target/release/*.so ./Horizon/plugins
