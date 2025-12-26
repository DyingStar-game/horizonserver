#!/bin/bash
set -e

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

# Update game_servers configuration
update_game_servers

# Execute the main application
exec /app/server "$@"
