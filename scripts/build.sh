#!/bin/bash

set -e


build() {
    cd "$1"
    cargo build --release
    cp target/release/*.so ../Horizon/plugins
    cd ..
}


mkdir -p "Horizon/plugins"

build "ds_game_server"
build "ds_player_authentication"
build "dyingstar_props"
