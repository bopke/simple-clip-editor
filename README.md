# Simple Clip Editor

Small and simple clip editor designed for easy clip creation for social media.

## Building

```sh
cargo build --release              # the app
cargo test --workspace             # the ffmpeg-backed tests skip if none is found
# on macos
./scripts/bundle-macos.sh          # "Clip Editor.app", ad-hoc signed and zipped
```

## ffmpeg

ffmpeg does all decoding and encoding as a child process. It is not
bundled: on first launch the app downloads a pinned build, verifies its
SHA-256, and installs it under the platform data directory
(`~/Library/Application Support/clip-editor/bin`,
`%LOCALAPPDATA%\clip-editor\bin`, `$XDG_DATA_HOME/clip-editor/bin`).

The version and checksum are pinned in
[`app/src/bootstrap.rs`](app/src/bootstrap.rs) — 9.0 from martin-riedl on
Apple Silicon, 8.1.2 from BtbN on Windows and Linux. Every pinned URL is
immutable, so a vendor cannot swap the bytes out from under the checksum.
If the download is not wanted, the first-run screen offers **Use ffmpeg from PATH** and a folder picker.

