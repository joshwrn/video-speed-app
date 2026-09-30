# Video Speed

Pick or drop a video, choose a speed (1.1x to 100x, or -1.1x to -100x to slow it down), optionally remove audio, and save the result as an MP4.

Requires `ffmpeg` and `ffprobe` on your PATH (`brew install ffmpeg`, `winget install ffmpeg`, or your Linux package manager).

## Install

Download the zip for your system from [Releases](../../releases).

- **macOS:** unzip and move Video Speed to Applications. The app isn't notarized, so macOS blocks the first launch: open System Settings → Privacy & Security and click Open Anyway.
- **Windows:** unzip and run `video-speed.exe`. If SmartScreen warns about an unrecognized app, click More info → Run anyway.

## Build

Rust is pinned in `rust-toolchain.toml`; rustup installs it on first build.

```sh
cargo run --release
```

To publish a release, push a version tag (`git tag v0.1.0 && git push origin v0.1.0`). GitHub Actions builds a universal macOS app and a Windows exe and attaches both to a new release.
