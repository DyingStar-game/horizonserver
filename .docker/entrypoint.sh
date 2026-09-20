#!/bin/bash
set -e

# plugins.toml ships in the horizon-data image and is mounted read-only (root-owned,
# copied by the copy-props init container) under /app/data. The plugins read it from
# the workdir (/app/plugins.toml — see ds_common::Config), so install a copy there.
# The monolith image (.docker/Dockerfile) still bakes /app/plugins.toml directly:
# without a data mount that copy is used as is.
#
# The Godot server pool is NOT resolved here any more: ds_game_server resolves
# `game_servers_dns` (or the GAME_SERVER_HOST env var) itself and re-reads DNS
# while running, so a godotserver rollout does not leave Horizon with dead IPs.
install_plugins_config() {
    local SRC="/app/data/plugins.toml"
    local DEST="/app/plugins.toml"

    if [ -f "$SRC" ]; then
        cp "$SRC" "$DEST"
        echo "Installed $DEST from $SRC"
    elif [ -f "$DEST" ]; then
        echo "No $SRC mounted, using $DEST baked in the image"
    else
        echo "ERROR: $DEST not found and no $SRC mounted." >&2
        echo "       plugins.toml ships in the horizon-data image: is dataImage.enabled in the chart," >&2
        echo "       and does the copy-props init container mount the data volume at /app/data?" >&2
        exit 1
    fi
}

install_plugins_config

# Execute the main application
exec /app/server "$@"
