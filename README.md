# file2video

Command-line tool that turns any file into an encrypted H.264 MP4 video and restores it byte for byte. The video format is compatible in both directions with [Bit2Vid v1](https://github.com/nuan-cmyk/Bit2Vid).

**How it works:** the file (and its name) is encrypted with AES-256-GCM, protected with Reed-Solomon error correction, drawn as black and white blocks on video frames, and encoded with x264. Decoding reverses the process and publishes the output only after the password and integrity checks pass.

## Requirements

- A prebuilt binary from [GitHub Releases](https://github.com/VulcanoSoftware/file2video/releases) (no Rust or Python needed).
- [FFmpeg](https://ffmpeg.org/download.html) with `libx264`, plus `ffprobe` — downloaded automatically on first use when it is missing (see [FFmpeg auto-download](#ffmpeg-auto-download)), or installed manually. A GPU with a hardware H.264 encoder is used automatically when available (for example, an AMD GPU on Windows uses AMF).

| Platform | Manual install |
| --- | --- |
| Windows x64 | `winget install --id Gyan.FFmpeg -e`, then reopen your terminal |
| Ubuntu / Debian | `sudo apt install ffmpeg` |
| macOS | `brew install ffmpeg` |

Check with `ffmpeg -version` and `ffprobe -version`. If FFmpeg is not on your `PATH`, pass `--ffmpeg /path/to/ffmpeg`; `ffprobe` is looked up next to it unless you set `--ffprobe`. FFmpeg versions before and after 5.1 are both supported.

## FFmpeg auto-download

`encode` and `decode` need FFmpeg, and GPU-accelerated encoding needs a build that includes hardware H.264 encoders. When the FFmpeg on the `PATH` is missing (or has no hardware encoders), the tool downloads a full static build on first use and caches it:

| System | Source | Cached in |
| --- | --- | --- |
| Windows x64 | [BtbN/FFmpeg-Builds](https://github.com/BtbN/FFmpeg-Builds/releases/tag/latest) (includes AMF, NVENC, Quick Sync) | `%LOCALAPPDATA%\file2video` |
| Linux x64 / ARM64 | [BtbN/FFmpeg-Builds](https://github.com/BtbN/FFmpeg-Builds/releases/tag/latest) (includes VA-API, NVENC, Quick Sync) | `$XDG_CACHE_HOME/file2video` or `~/.cache/file2video` |
| macOS | [evermeet.cx](https://evermeet.cx/ffmpeg/) | as on Linux |

- Downloading needs `curl` (shipping with Windows 10+, macOS, and most Linux distributions) and `tar`. Archives are fetched over HTTPS from the sources above.
- The hardware encoder support is compiled into the downloaded FFmpeg build. GPU **drivers** come from your GPU vendor and are never installed by this tool.
- Set the `FILE2VIDEO_CACHE` environment variable to choose the cache location, pass `--no-download` to keep everything local, or `--ffmpeg <PATH>` to use a specific FFmpeg. When no download is possible, the tool continues with the FFmpeg on your system as before.
- The download happens once; later runs reuse the cached build. Delete the cache directory to force a fresh download of the current build.

## Install

Download the archive for your system from [Releases](https://github.com/VulcanoSoftware/file2video/releases), verify it against the `.sha256` file next to it, and extract it.

| System | Archive |
| --- | --- |
| Windows x64 | `file2video-<version>-windows-x86_64.zip` |
| Linux x64 / ARM64 | `file2video-<version>-linux-x86_64.tar.gz` / `-linux-aarch64.tar.gz` |
| macOS Intel / Apple Silicon | `file2video-<version>-macos-x86_64.tar.gz` / `-macos-aarch64.tar.gz` |

The Linux x64 build is made on Ubuntu 22.04 and the Linux ARM64 build on Ubuntu 24.04. On older distributions, [build from source](#build-from-source).

## Quick start

```sh
./file2video estimate archive.zip                  # frames and duration, writes nothing
./file2video encode archive.zip archive.mp4        # asks for a password (twice)
mkdir recovered
./file2video decode archive.mp4 recovered          # asks for the password, restores archive.zip
```

On Windows, use `.\file2video.exe` in PowerShell. To check the result:

```powershell
Get-FileHash .\archive.zip, .\recovered\archive.zip
```

Decode the result once after encoding to confirm that the video works, especially when you use non-default settings.

## Commands

### `encode <INPUT> <OUTPUT>`

Encrypts `INPUT` and writes the video to `OUTPUT`. `OUTPUT` must not exist yet.

| Option | Default | Description |
| --- | --- | --- |
| `--width`, `--height` | `1920`, `1080` | Storage grid that sets the per-frame capacity. Even numbers, at most 8192 per side and 35.4 megapixels, divisible by the block size. The MP4 itself is stored at cell resolution (256×256 or larger, one cell per pixel) and upscaled while viewing. |
| `--block-size` | `8` | Pixels per bit. One of `8`, `10`, `12`, `16`, `20`. Larger blocks hold less data per frame, so the video gets longer. |
| `--fps` | `30` | Frames per second (1–60). |
| `--crf` | `18` | x264 quality (0–51, lower is better). In default compact mode the quality is capped at 10 because every cell must survive H.264 quantization exactly. |
| `--preset` | `slow` | x264 preset, `ultrafast` to `veryslow`. |
| `--ecc-symbols` | `64` | Reed-Solomon parity bytes per 255-byte codeword (1–127). |
| `--v1` | off | Store only black/white cells (2 luma levels) in the old full-resolution Bit2Vid v1 layout, so the original Python Bit2Vid 1.0 can still decode the video. Decoding detects all formats automatically. |
| `--compress` | off | Smaller-output profile: `light`, `medium` or `high`. |
| `--pbkdf2-iterations` | `600000` | PBKDF2-HMAC-SHA256 iterations (1–10,000,000). |

`--compress` sets CRF, preset and parity together: `light` = CRF 22 / `slow` / 64, `medium` = CRF 30 / `slower` / 32, `high` = CRF 38 / `veryslow` / 16. Explicit `--crf`, `--preset` and `--ecc-symbols` override the profile. These profiles reduce the error-correction margin.

### `decode <INPUT> <OUTPUT>`

Recovers the original file from the video. The resolution, block size and cell depth (16 luma levels or plain black/white) are detected automatically.

- If `OUTPUT` is an existing directory, the file is restored there under its original name.
- If `OUTPUT` is a file path, the file is written to that name. With `--restore-name` the original name is used instead, in the directory that contains `OUTPUT`.
- Existing files are never overwritten.
- The result appears at its final path only after the AES-GCM tag and SHA-256 hash have been verified. On failure, the temporary file is removed.
- A stored filename that is unsafe to restore (path separators, Windows device names, control characters and similar) is rejected; give an explicit output file name instead.

| Option | Description |
| --- | --- |
| `--restore-name` | Use the stored filename, in the directory that contains `OUTPUT`. |
| `--width`, `--height` | Override resolution detection (both required together). |
| `--block-size` | Override block-size detection. |

### `estimate <INPUT>`

Prints the exact encrypted size, frame count and playback duration for a file, without writing any video. It accepts the same video options as `encode`. It cannot predict the MP4 file size or the encoding time.

### Options for all commands

| Option | Description |
| --- | --- |
| `--ffmpeg <PATH>` | FFmpeg executable (default: from `PATH`, auto-downloaded when missing). |
| `--no-download` | Never download FFmpeg; use what is already on the system. |
| `--ffprobe <PATH>` | ffprobe executable (default: next to FFmpeg, else `PATH`). |
| `--gpu <MODE>` | Hardware encoder for `encode`: `auto` (default), `off`, `amf`, `nvenc`, `qsv`, `vaapi`, `video-toolbox`. See [GPU acceleration](#gpu-acceleration). |
| `--threads <N>` | Thread limit for the Rust error-correction pool and for FFmpeg (`0` = automatic). |

## Diagnostics

`encode` and `decode` print a diagnostics block to stderr before starting:

```text
System: windows x86_64 | AMD Ryzen 5 230 (16 threads) | 15.8 GB
GPU: AMD Radeon 760M (driver 32.0.11021.2)
FFmpeg: ffmpeg version N-118233-g... | hardware encoders: ["h264_amf", "h264_nvenc", "h264_qsv"]
```

During long runs, progress lines show frames, percentage, throughput in MB/s, elapsed time and ETA:

```text
Encode: 90000/353959 frames (25.4%) | 21.3 MB/s | elapsed 1m07s | eta 3m18s
```

Set `FILE2VIDEO_DEBUG=1` to also print the exact FFmpeg command used for each GPU trial encode, which is useful when reporting a problem.

## Passwords

`encode` asks for a password and a confirmation; `decode` asks once. For scripts, use `--password-file <FILE>`: a UTF-8 text file of at most 64 KiB, of which one trailing newline is ignored. `--password <TEXT>` also works but can leak through shell history and process lists. The two options cannot be combined, and an empty password is rejected.

For Bit2Vid v1 compatibility, passwords can only contain Latin-1 characters (for example ASCII and `é`). Other characters are rejected.

## Size, duration and disk space

- **Capacity:** by default each pixel of the video stores one cell with 4 bits (one of 16 brightness levels), so one frame carries 32,768 bytes at the standard 256×256 cell grid, of which about 24,500 are file data after error correction. That is roughly 735 KB per second of video. 1 MiB takes 41 frames (1.4 s); 1 GiB takes about 41,000 frames (23 minutes). Pass `--v1` for the old Bit2Vid v1 layout (full-resolution 1080p frame, one black/white bit per 8×8 block, 4,050 bytes per frame, 3.3 hours of video per GiB).
- **MP4 size:** the video is stored at cell resolution (256×256 or larger, one cell per pixel) and only upscaled during playback for viewing, so the MP4 tracks the payload size instead of ballooning several times larger like a full-resolution version would. Compact mode encodes at capped quality (`--crf` is capped at 10) because every cell must survive H.264 quantization intact; for random or compressed data — which H.264 cannot compress — expect roughly 2× the input (versus 5× at full resolution with default quality). Use `estimate` for the duration, not the file size.
- **File size limit:** a single file must be just under 64 GiB (the AES-GCM limit of Bit2Vid v1).
- **Disk space:** `encode` stages the ciphertext in a temporary file in the output directory, because Bit2Vid stores the authentication tag before the ciphertext. Budget about the input size for that, plus the MP4 itself. `decode` stages the restored file in the output directory before publishing it.
- **Speed:** encoding uses a hardware GPU encoder when one is available (see [GPU acceleration](#gpu-acceleration)); otherwise x264 at the default `slow` preset dominates encoding time. `--preset veryfast` is faster at the cost of a different size/quality trade-off; verify the result with a round trip. Error correction runs in parallel and uses AVX2 on x86-64 when available, with a portable fallback.

## GPU acceleration

`encode` uses the hardware H.264 encoder on your GPU by default. A small trial encode checks each candidate in order and the first working one is used; if none works, a notice is printed and the video is encoded on the CPU with `libx264` as before.

| System | Backends tried in order | Values for `--gpu` |
| --- | --- | --- |
| Windows | AMF (AMD), NVENC (NVIDIA), Quick Sync (Intel) | `amf`, `nvenc`, `qsv` |
| Linux | VA-API (`/dev/dri/renderD128`), NVENC, Quick Sync | `vaapi`, `nvenc`, `qsv` |
| macOS | VideoToolbox | `video-toolbox` |

- Hybrid laptops: when a discrete GPU is detected (any NVIDIA card, Intel Arc, or an AMD Radeon RX model), its encoder is tried before the integrated GPU's.
- A GPU is never required: without a working hardware encoder, or with `--gpu off`, the video is encoded on the CPU with `libx264`. FFmpeg still needs `libx264` for that fallback path.
- A specific `--gpu` value forces that backend and fails if it is not available in your FFmpeg build or its trial encode fails.
- The GPU encoder is used for encoding only; decoding always runs on the CPU.
- `--crf` and `--preset` are translated per backend: constant QP on AMF and VA-API, `-cq` on NVENC, ICQ quality on Quick Sync, and a quality setting on VideoToolbox. The presets map to each encoder's speed levels.
- Verify the result with a round trip (`encode`, then `decode`, then compare hashes), especially when you use non-default `--crf`, `--preset` or `--compress` settings.
- Troubleshooting: when every backend fails, each one prints its FFmpeg trial error (for example `AMF_NO_DLL` means the AMD Adrenalin driver is missing or too old). Update the GPU driver and run again; use `--gpu off` to encode on the CPU without diagnostics.

## Reliability and security

- **Error correction:** Reed-Solomon (255 bytes per codeword) repairs up to half the parity count in corrupted bytes per codeword, which is 32 bytes at the default of 64. It repairs damaged pixels, not missing frames.
- **The video is not a backup:** cropping, scaling, trimming, dropped frames or heavy re-encoding (for example by a video platform) can make recovery impossible. Keep the original file.
- **Encryption:** AES-256-GCM with a key from PBKDF2-HMAC-SHA256 (600,000 iterations by default) and a random salt and nonce per file. The original filename is encrypted along with the data. A wrong password or a damaged video fails authentication, and no output is written.
- **What is not hidden:** the video visibly shows black, white and grey blocks, and its length reveals the approximate file size.

## Build from source

You need a recent stable Rust toolchain (the version used by CI is pinned in [`ci.yml`](.github/workflows/ci.yml)) and FFmpeg with `libx264` for the tests.

```sh
cargo build --release --locked      # binary: target/release/file2video
cargo test --release --locked
cargo clippy --locked --all-targets -- -D warnings
```

Optional error-correction throughput measurement:

```sh
cargo test --release --locked ecc_throughput -- --ignored --nocapture
```

CI builds and tests Windows x64, Linux x64/ARM64 and macOS x64/ARM64. On Linux x64 it also runs `tests/interoperability.py`, which checks Python → Rust and Rust → Python against a pinned Bit2Vid commit (needs `numpy`, `pycryptodome`, `reedsolo` and `tqdm`; see the CI workflow for the versions).

## Releases

- Every push to `main` that passes all platform builds publishes a release tagged `build-<commit>` with all five archives and their SHA-256 files. A rerun never creates a duplicate.
- Pushing a tag `vX.Y.Z` that matches the version in `Cargo.toml` publishes a numbered release.
- Pull request builds are only available as temporary [Actions artifacts](https://github.com/VulcanoSoftware/file2video/actions/workflows/ci.yml).
