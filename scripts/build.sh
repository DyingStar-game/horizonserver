#!/bin/bash

set -e

mkdir -p "Horizon/plugins"
RUSTFLAGS="" cargo build --release
cp target/release/*.so ./Horizon/plugins
