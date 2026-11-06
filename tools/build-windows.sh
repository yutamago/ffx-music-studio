#!/usr/bin/env bash
# Builds ffx-music-studio.exe (x86_64-pc-windows-msvc) on Linux/WSL with
# cargo-xwin, static CRT (no Visual C++ runtime needed).
# Needs: rustup target add x86_64-pc-windows-msvc; cargo install cargo-xwin.
# Usage: tools/build-windows.sh [extra cargo args]
set -euo pipefail
repo="$(cd "$(dirname "$0")/.." && pwd)"
# lld-link -> rustc's rust-lld (Ubuntu's lld-link can be too old)
export PATH="$repo/tools/shims:$PATH"
export XWIN_ARCH=x86_64
export RUSTC_WRAPPER=
target=x86_64-pc-windows-msvc
cache="${XDG_CACHE_HOME:-$HOME/.cache}"
mkdir -p "$cache"
# Setting up cargo-xwin's MSVC CRT cache races between parallel builds, so
# that runs under a lock (the same one ffx-demaster's build uses).
eval "$(flock "$cache/ffx-xwin.lock" cargo xwin env --target "$target" 2>/dev/null)"
# cargo-xwin sets RUSTFLAGS (empty), which overrides .cargo/config.toml's
# target rustflags: link the CRT statically here too.
export RUSTFLAGS="${RUSTFLAGS:-} -C target-feature=+crt-static"
cd "$repo"
cargo build --release --target "$target" "$@"
echo "built: $repo/target/$target/release/ffx-music-studio.exe"
