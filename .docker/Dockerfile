# ------------------------ #
# ---- Stage 1: Build ---- #
# ------------------------ #
FROM rust:1.94.1-bookworm AS builder

WORKDIR /build

# Copy the full source tree (both workspaces)
COPY Cargo.toml Cargo.toml
COPY Cargo.lock Cargo.lock
COPY ds_common/ ds_common/
COPY ds_game_server/ ds_game_server/
COPY ds_player_authentication/ ds_player_authentication/
COPY ds_genericprops/ ds_genericprops/
COPY ds_props/ ds_props/
COPY ds_services/ ds_services/
COPY ds_audio/ ds_audio/
COPY ds_bridge/ ds_bridge/
COPY Horizon/ Horizon/
COPY scripts/ scripts/

# Build the horizon binary from the Horizon workspace
RUN cd Horizon && cargo build --release

# Build the plugins from the ds_* crates
RUN scripts/build.sh

# -------------------------- #
# ---- Stage 2: Runtime ---- #
# -------------------------- #
FROM ubuntu:22.04

ARG UID=1000
ARG GID=1000

# Install required tools
RUN apt-get update && apt-get install -y --no-install-recommends \
    passwd \
    dnsutils \
    && rm -rf /var/lib/apt/lists/*

# Create a non-privileged user that the app will run under.
RUN groupadd --gid $GID appgroup \
    && useradd --uid $UID --gid $GID -m appuser

# Create app directory
RUN mkdir -p /app

# Copy plugins built from ds_* crates
COPY --from=builder /build/Horizon/plugins /app/plugins

# Copy config
COPY .docker/production_config.toml /app/config.toml
COPY .docker/plugins.toml /app/plugins.toml

# Copy props definition folder (genericprops)
COPY ds_genericprops/props /app/props
COPY ds_genericprops/startup_items.json /app/startup_items.json

# Copy server binary
COPY --from=builder /build/Horizon/target/release/horizon /app/server

# Copy entrypoint script
COPY .docker/entrypoint.sh /app/entrypoint.sh

# Set permissions for user
RUN chown -R appuser:appgroup /app && chmod +x /app/entrypoint.sh

# Switch to non-privileged user
USER appuser

# Expose the port that the application listens on.
EXPOSE 7040

WORKDIR /app

RUN ls /app/server

ENTRYPOINT [ "/app/entrypoint.sh" ]