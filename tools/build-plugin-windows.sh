#!/usr/bin/env bash
# Builds FFX PS2 Sound (plugin/, crate ffx-spu2-plugin) for Windows x86_64 on
# Linux/WSL with cargo-xwin (static CRT) and puts the bundles together:
#   target/bundled/FFX PS2 Sound.vst3/Contents/x86_64-win/FFX PS2 Sound.vst3
#   target/bundled/FFX PS2 Sound.clap
# (the layout nih-plug's bundler makes; both are the same DLL).
# Needs: rustup target add x86_64-pc-windows-msvc; cargo install cargo-xwin.
# Usage: tools/build-plugin-windows.sh [extra cargo args]
set -euo pipefail
repo="$(cd "$(dirname "$0")/.." && pwd)"
export PATH="$repo/tools/shims:$PATH"
export XWIN_ARCH=x86_64
export RUSTC_WRAPPER=
target=x86_64-pc-windows-msvc
cache="${XDG_CACHE_HOME:-$HOME/.cache}"
mkdir -p "$cache"
eval "$(flock "$cache/ffx-xwin.lock" cargo xwin env --target "$target" 2>/dev/null)"
export RUSTFLAGS="${RUSTFLAGS:-} -C target-feature=+crt-static"
cd "$repo"
cargo build --release --target "$target" -p ffx-spu2-plugin --lib --features editor "$@"
dll="target/$target/release/ffx_spu2_plugin.dll"
name="FFX PS2 Sound"
out="target/bundled"
rm -rf "$out/$name.vst3" "$out/$name.clap"
mkdir -p "$out/$name.vst3/Contents/x86_64-win"
cp "$dll" "$out/$name.vst3/Contents/x86_64-win/$name.vst3"
cp "$dll" "$out/$name.clap"
echo "built: $repo/$out/$name.vst3 and $repo/$out/$name.clap"
