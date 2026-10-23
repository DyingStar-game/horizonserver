#!/bin/bash

set -e

# Local dev: player tokens decoded without signature verification, unless the
# environment says otherwise (VERIFY_JWT_SIGNATURE=true + OIDC_ISSUER, see README).
export VERIFY_JWT_SIGNATURE="${VERIFY_JWT_SIGNATURE:-false}"

cd Horizon
cargo run --release --bin horizon -- --config dev_config.toml
