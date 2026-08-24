#!/bin/bash
#
# Checks out the Horizon submodule at the commit pinned by this repository and
# drops in the runtime configs.
#
# Horizon lives at github.com/DyingStar-game/Horizon: `main` mirrors upstream,
# our patches sit on `ds-develop` / `ds-main`. The commit built here is the one
# recorded in the `Horizon` gitlink, so CI and local dev build the same tree.
# See docs/horizon-fork.md to move that pin.

set -euo pipefail

cd "$(dirname "$0")/.."

git submodule sync --recursive
git submodule update --init --recursive Horizon

# Runtime configs are kept here rather than in the fork, so that rebasing our
# patches onto upstream never conflicts on a config file. Horizon reads both
# relative to its own working directory (see scripts/run.sh).
cp plugins.toml Horizon/plugins.toml
cp dev_config.toml Horizon/dev_config.toml
