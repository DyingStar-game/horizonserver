# Working with the Horizon fork

The Horizon server source lives in [DyingStar-game/Horizon](https://github.com/DyingStar-game/Horizon),
a fork of [Far-Beyond-Dev/Horizon](https://github.com/Far-Beyond-Dev/Horizon).
This repo consumes it through the `Horizon/` submodule.

## Branch layout

| Branch       | Role |
| ------------ | ---- |
| `main`       | Strict mirror of upstream. Never commit here — a daily GitHub Action fast-forwards it. |
| `ds-develop` | Our patches, rebased on top of `main`. This is the fork's default branch and what `horizonserver`'s `develop` pins. |
| `ds-main`    | Release line. Fast-forwarded from `ds-develop` when we cut a `horizonserver` release. |

The pinned commit is the `Horizon` gitlink recorded in this repo — that is what
CI and every developer build, so there is no way for local and CI to drift.

The mirror workflow lives on `ds-develop` rather than `main` because GitHub only
runs scheduled workflows from a repository's default branch, and `main` has to
stay byte-identical to upstream so our rebases never conflict.

## Adding a patch to Horizon

```bash
cd Horizon
git checkout ds-develop
# ... edit, then commit as one focused commit per change ...
git push origin ds-develop

cd ..
git add Horizon
git commit -m "chore(horizon): bump submodule"
```

Keep the commits small and single-purpose. They get replayed on every upstream
bump, and a patch that stays legible is a patch that can be sent upstream.

Never leave uncommitted changes inside `Horizon/`: the submodule pin only
records commits, so anything uncommitted is invisible to CI and silently absent
from the images.

## Pulling in upstream changes

```bash
cd Horizon
git fetch origin && git fetch upstream

# 1. main is mirrored daily by the "Sync upstream main" action.
#    To force it now: gh workflow run ds-sync-upstream.yml -R DyingStar-game/Horizon
#    Or by hand:      git push origin upstream/main:refs/heads/main

# 2. Replay our patches on the new upstream.
git checkout ds-develop
git rebase origin/main
cargo build --release            # resolve conflicts, then check it still builds
git push --force-with-lease origin ds-develop

# 3. Move the pin in this repo.
cd ..
git add Horizon
git commit -m "chore(horizon): bump submodule to <sha>"

# 4. Re-sync the duplicated dependency versions (see below).
scripts/check_deps_sync.sh
```

`ds-develop` and `ds-main` are force-pushed after a rebase. That is safe here:
the only consumer is this repository, and it pins by SHA rather than by branch.

## The duplicated dependency block

`Cargo.toml` at the root of this repo carries a `[workspace.dependencies]`
section copied by hand from `Horizon/Cargo.toml`. It has to exist because the
`ds_*` plugins are a separate cargo workspace, but the plugin cdylibs and the
host binary must link identical versions of the shared crates — tokio above all.
A mismatch does not fail the build; it fails at runtime, when a runtime handle
created by the host is used inside a plugin.

`scripts/check_deps_sync.sh` compares the two and fails on any divergence. Run
it after every bump, and remember it only checks crates named on both sides.

## Upstream's own workflows

The fork inherited upstream's `.github/workflows/` (`release.yml`,
`docker-publish.yml`, `rust.yml`, `devskim.yml`). They all trigger on `main`,
which our mirror pushes to daily. They are disabled in the fork's Actions
settings and must stay that way — otherwise every mirror push would cut a
release and publish a GHCR image from our fork. The files themselves are left
untouched on purpose, so rebasing onto upstream never conflicts on them.
