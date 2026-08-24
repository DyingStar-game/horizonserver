#!/bin/bash

set -e

cd Horizon
cargo run --release --bin horizon -- --config dev_config.toml
