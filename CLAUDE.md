# ffx-music-studio

FFX Music Studio: plays, edits (in memory) and exports the PS2 music of the
player's own FFX disc. Everything about it: `README.md`.

## Rules

- Never commit game data: no disc images, `.bgm`/`.wd` files, rendered
  audio (`.wav`), exports (`.mid`, `.sf2`), screenshots. The studio reads
  the player's own disc; the repository holds code only. No machine paths,
  tokens or private notes in files either (the repository is public).
- Never add a `Co-Authored-By: Claude` footer (or any Claude attribution) to
  commits, PR descriptions or other messages.
- One copy of the engine: `sqseq` (the `iopsound.irx` sequencer) and
  `spu2` stay in ffx-demaster's `converter/` (the `demaster` crate, a git
  dependency pinned by `rev` in `Cargo.toml`). Engine changes go into
  ffx-demaster first (it keeps the public API this crate uses), then bump
  the `rev`. For local work on both, uncomment the `[patch]` at the end of
  `Cargo.toml`. `ffx-vbf` stays at ffx-demaster's ffx-rust rev.
- Check results by listening and by measurement (renders, spectra), and
  compare with the PS2 driver's own render, not with what seems plausible.

## Build

- Rust: build with `-j 4`; if cargo stalls on `sccache`, `RUSTC_WRAPPER=`.
- Both git dependencies are private: `.cargo/config.toml` fetches through
  the git CLI (its credentials).
- `cargo test`; `FFX_STUDIO_CONFIG=<demaster.toml>` adds the disc tests
  (a `demaster.toml` whose `[source] iso` is the PS2 image and `game` the
  Steam HD Remaster).
- Windows exe from Linux/WSL: `tools/build-windows.sh` (cargo-xwin, static
  CRT; `tools/shims/lld-link` forwards to rustc's rust-lld).
- Python checks: `uv run --with <deps> python -I script.py`.
- Paths with `(`/`)` (a typical ISO name) upset some sandboxes: symlink the
  ISO to a plain path.

## Running the Windows exe from WSL

- It is a GUI program: a WSL `timeout` does not end the Windows process.
  End the one you started with `taskkill.exe /PID <pid> /F`, never by name
  (other instances may be running).
- Windows processes see WSL symlinks through `\\wsl.localhost` as missing:
  give them real paths.
- Output of Windows executables is CRLF: strip `\r` before comparing.
