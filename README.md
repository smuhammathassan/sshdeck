# sshdeck

A GPU-accelerated SSH / SFTP desktop client written in Rust, built on
[GPUI](https://github.com/zed-industries/zed) via
[gpui-kit](https://github.com/longbridge/gpui-kit).

No Electron, no Chromium, no bundled Node runtime. The goal is a full-featured
terminal workspace that idles at tens of megabytes and does not spin a core
when nothing is happening.

[![CI](https://github.com/smuhammathassan/sshdeck/actions/workflows/ci.yml/badge.svg)](https://github.com/smuhammathassan/sshdeck/actions/workflows/ci.yml)

## Status

Early. The window shell exists — host inventory, filtering, selection,
persistence, theming. The SSH transport is not wired up yet.

## Layout

```
crates/sshdeck-core/   domain model, host store, vault, SSH/SFTP client. No UI.
crates/sshdeck-app/    GPUI application (binary: sshdeck).
```

`sshdeck-app` depends on `sshdeck-core`. Core never depends on the UI.

## Building

Everything is built in CI. To build locally you need a recent stable Rust
toolchain and the macOS SDK:

```
cargo build -p sshdeck-app --release
cargo run -p sshdeck-app --release
```

Core has no GUI dependencies and builds anywhere:

```
cargo test -p sshdeck-core
```

## Not affiliated

An independent, clean-room implementation. Not affiliated with, endorsed by, or
derived from Termius. It interoperates over standard protocols (SSH2, SFTP,
FIDO2) and contains no third-party application code, assets, or branding.

## License

MIT
