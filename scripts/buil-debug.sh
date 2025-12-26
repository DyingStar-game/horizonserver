#!/bin/bash

set -e

mkdir -p "Horizon/plugins"
# RUSTFLAGS="--cfg  tokio_unstable" cargo build
RUSTFLAGS="" cargo build
cp target/debug/*.so ./Horizon/plugins
