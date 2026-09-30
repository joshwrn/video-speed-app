# Video Speed

Pick or drop a video, choose a speed (1.1x to 100x, or -1.1x to -100x to slow it down), optionally remove audio, and save the result as an MP4.

Requires `ffmpeg` and `ffprobe` on your PATH (`brew install ffmpeg`, `winget install ffmpeg`, or your Linux package manager). Rust is pinned in `rust-toolchain.toml`; rustup installs it on first build.

```sh
cargo run --release
```
