# batchviproc

Desktop tool for batch video compression on Windows. It scans a folder (including subfolders) for videos and recompresses them with FFmpeg, using NVIDIA NVENC (AV1, HEVC, H.264) or x264 on the CPU. Original files are replaced only when the new file is smaller, and their dates and attributes are preserved.

[Releases](https://github.com/Guilherme-Weber/batchviproc/releases)

## Origin

This project is a fork of [batchviproc](https://github.com/kachurovskiy/batchviproc) by **Maxim Kachurovskiy**, and it is strongly based on that program. The core workflow comes from the original application:

- recursive folder scanning;
- a queue processed from the largest file first;
- processed history in `processedFiles.txt`;
- replacing the original only when the output is smaller;
- preserving file dates.

The quality and speed settings and the FFmpeg parameters also come from it.

Version 2.0 rewrites the application in Rust and adds the features listed below. The processed history file remains compatible, so files already handled by the original version are not processed again.

The original work is distributed under the MIT License (see [LICENSE](LICENSE)), which this fork keeps.

## What is new in version 2.0

**Platform**

- Native Rust application with a [Slint](https://slint.dev) interface in the Windows 11 Fluent style, replacing Electron. It uses about 15–30 MB of memory.
- Single executable plus `ffmpeg.exe`, with no installation and no runtime dependencies.

**Encoding**

- GPU encoding with NVENC (AV1 10-bit, HEVC 10-bit, H.264), plus x264 for systems without an NVIDIA GPU.
- Parallel encoding with 1 to 4 jobs. Before starting each additional encoder, the app checks free memory, so parallel 4K encodes wait instead of exhausting RAM.
- Automatic fallbacks when a file is rejected: AAC audio transcoding, then CPU encoding.
- More formats: `.mp4`, `.mkv`, `.mov`, `.webm`, `.avi`, `.wmv`, `.mts`, `.m2ts`, `.flv`, `.m4v`, `.ts`, `.3gp`, `.mpg`, `.mpeg` and animated `.gif`. Legacy containers are converted to `.mp4`.
- Handling for real-world problem files. Each case is covered by regression tests in `src/engine_tests.rs`:
  - WMV/AVI/FLV with WMA or PCM audio;
  - videos that change resolution mid-stream;
  - tiny sticker GIFs and static spacer GIFs;
  - audio-only MP4 files;
  - corrupt downloads.
- A stall watchdog that ends FFmpeg processes that stop making progress.

**Control and monitoring**

- **Pause/Resume**: pausing suspends the running FFmpeg processes without losing progress. No new files start while paused, and the PC is allowed to sleep. Resume continues the same encodes.
- Live progress for each worker: FPS, speed, NVENC load, VRAM, space saved and failures.
- Progress shown on the Windows taskbar button.
- Restores the read-only flag along with the creation, modification and access dates.
- Keeps the PC awake while a batch runs.
- A persistent activity log and crash reports, with a link to open the log folder.
- Only one instance can run at a time.
- Command-line options for scheduled runs.

## Requirements

- Windows 10 or 11 (64-bit).
- For GPU encoding, an NVIDIA GPU with NVENC. AV1 requires an RTX 40 series or newer.

## Usage

1. Extract the release zip and run `batchviproc.exe`.
2. Choose a folder.
3. Select the encoder, number of jobs, quality and speed.
4. Click **Start**. Use **Pause/Resume** or **Stop** at any time.

### Command line

```powershell
batchviproc.exe [--start] [folder]
```

Opens `folder` directly. With `--start`, processing begins immediately with the last-used settings, which is useful with Windows Task Scheduler.

## Data files

All data is stored in `%APPDATA%\batchviproc`:

- `processedFiles.txt`: the processed history. It uses the same format as the original version, and new entries are appended.
- `ui-settings.json`: the last encoder settings.
- `logs\batchviproc.log`: the activity log with timestamps. It is rotated to `batchviproc.1.log` at 5 MB.
- `logs\crash-*.txt`: crash reports, written only if the application fails.

## Development

Requirements:

- [Rust](https://rustup.rs) (stable, MSVC toolchain);
- Visual Studio Build Tools with the C++ workload.

```powershell
git clone https://github.com/Guilherme-Weber/batchviproc.git
cd batchviproc
powershell -ExecutionPolicy Bypass -File scripts/fetch-ffmpeg.ps1   # downloads FFmpeg into bin/
cargo run
```

Project layout:

- `src/engine.rs`: queue, workers, fallbacks, pause and memory control.
- `src/args.rs`: FFmpeg arguments.
- `src/app.rs`: the bridge between the engine and the interface.
- `ui/app.slint`: the interface.

`scripts/fetch-ffmpeg.ps1` downloads the pinned FFmpeg "essentials" build (currently 9.0.2) from gyan.dev. It verifies the checksum and checks that the required encoders are present. To evaluate another version:

1. Run the script with `-Force -Version <x.y.z>` (or `-Version latest`).
2. Run the tests.
3. Update `$PinnedVersion` in the script.

Tests:

```powershell
cargo test
```

The end-to-end tests encode real videos with `bin/ffmpeg.exe`, and the NVENC cases require an NVIDIA GPU.

## Building

```powershell
powershell -ExecutionPolicy Bypass -File scripts/package.ps1
```

This produces `dist/batchviproc-<version>-win64.zip`, containing `batchviproc.exe` and `ffmpeg.exe`. The version is defined in `Cargo.toml`.

## License

MIT. Original work copyright (c) 2021 Maxim Kachurovskiy. See [LICENSE](LICENSE).

FFmpeg is distributed under its own license, included in the release as `ffmpeg-LICENSE.txt`.
