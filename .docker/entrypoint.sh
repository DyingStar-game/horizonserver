#!/bin/bash
set -e

# plugins.toml ships in the horizon-data image and is mounted read-only (root-owned,
# copied by the copy-props init container) under /app/data. The plugins read it from
# the workdir (/app/plugins.toml — see ds_common::Config) and update_game_servers
# rewrites it with `sed -i`, so install a private copy the server user can write.
# The monolith image (.docker/Dockerfile) still bakes /app/plugins.toml directly:
# without a data mount that copy is used as is.
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

# Function to update game_servers in plugins.toml
update_game_servers() {
    local PLUGINS_FILE="/app/plugins.toml"
    
    if [ -n "$GAME_SERVER_HOST" ]; then
        echo "Using GAME_SERVER_HOST=$GAME_SERVER_HOST"
        GAME_SERVERS_LINE="game_servers = [\"ws://${GAME_SERVER_HOST}:8980\"]"
        echo "Setting: $GAME_SERVERS_LINE"
        sed -i "s|^game_servers = .*|${GAME_SERVERS_LINE}|" "$PLUGINS_FILE"
        echo "Updated $PLUGINS_FILE successfully"
        return
    fi

    echo "Running nslookup for godotserver..."
    
    # Run nslookup and extract IP addresses
    # Filter lines that start with "Address:" but exclude the DNS server address (contains #53)
    IPS=$(nslookup godotserver 2>/dev/null | grep "Address:" | grep -v "#53" | awk '{print $2}')
    
    if [ -z "$IPS" ]; then
        echo "Warning: No IP addresses found for godotserver, keeping default configuration"
    else
        echo "Found IP addresses:"
        echo "$IPS"
        
        # Build the game_servers array string
        SERVERS=""
        for IP in $IPS; do
            if [ -z "$SERVERS" ]; then
                SERVERS="\"ws://${IP}:8980\""
            else
                SERVERS="${SERVERS}, \"ws://${IP}:8980\""
            fi
        done
        
        GAME_SERVERS_LINE="game_servers = [${SERVERS}]"
        echo "Setting: $GAME_SERVERS_LINE"
        
        # Update the plugins.toml file using sed
        # Replace the existing game_servers line with the new one
        sed -i "s|^game_servers = .*|${GAME_SERVERS_LINE}|" "$PLUGINS_FILE"
        
        echo "Updated $PLUGINS_FILE successfully"
    fi
}

# Install plugins.toml, then update its game_servers configuration
install_plugins_config
update_game_servers

# Execute the main application
exec /app/server "$@"
