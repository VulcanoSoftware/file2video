# file2video

Command-line tool that turns any file into an encrypted H.264 MP4 video and restores it byte for byte. Encryption is the default and can be turned off with `--no-password`.

**How it works:** the file (and its name) is encrypted with AES-256-GCM (optional with `--no-password`), protected with Reed-Solomon error correction, drawn as cells with 16 brightness levels on video frames (optionally also in the color planes), and encoded with x264 or a hardware GPU encoder. Decoding reverses the process and publishes the output only after the password and integrity checks pass.

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
| `--crf` | `8` | x264 quality (0–51, lower is better). This mode caps the quality at 8 (6 with `--intra`) because every cell must survive H.264 quantization exactly. |
| `--preset` | `faster` | x264 preset, `ultrafast` to `veryslow`; also selects the speed tier of the hardware GPU encoders. |
| `--chroma` | off | Also store data in the U and V color planes: +50% capacity per frame with about the same compression, so roughly a third fewer frames and a third faster to encode and decode. Verify with `benchmark` or a round trip, since hardware encoders handle color planes differently. |
| `--intra` | off | All-intra encoding (`-g 1`): every frame is independent, which makes decoding 20–40% faster at the cost of a somewhat larger MP4. The quality cap drops to 6 to keep the error-correction margin. |
| `--ecc-symbols` | `32` | Reed-Solomon parity bytes per 255-byte codeword (1–127). |
| `--compress` | off | Smaller-output profile: `light`, `medium` or `high`. |
| `--pbkdf2-iterations` | `600000` | PBKDF2-HMAC-SHA256 iterations (1–10,000,000). |
| `--no-password` | off | Store the file without encryption: anyone who downloads the video can read it. A SHA-256 integrity check still protects against damaged videos; decoding never asks for a password. |
| `--optimise <TARGET>` | off | Pick the best measured backend and frame mode for this machine: `smallest`, `fastest` or `balanced`. See [Calibration and automatic optimisation](#calibration-and-automatic-optimisation). |
| `--stealth <VIDEO>` | off | Hide the data as a faint luma layer over a background video instead of storing cells; decoding is blind and does not need that video again. See [Stealth mode](#stealth-mode). |

`--compress` sets preset and parity together: `light` = `slow` / 64, `medium` = `slower` / 32, `high` = `veryslow` / 16; CRF stays capped at 8 (6 with `--intra`). Explicit `--crf`, `--preset`, `--chroma`, `--intra` and `--ecc-symbols` override the profile. These profiles reduce the error-correction margin.

### `benchmark [INPUT]`

Measures every relevant configuration on this machine: each available backend × each mode (default, `--intra`, `--chroma`, both), then prints a table with frames, encode/decode wall time and speed in MB/s, MP4 size and data/MP4 efficiency. Without a positional INPUT file it generates pseudo-random payload of `--size` MiB (default 8). Not affected by `--compress`; the password/PBKDF2 speed is not representative because a fixed low iteration count is used.

```sh
./file2video benchmark                          # all backends x all modes on an 8 MiB payload
./file2video benchmark --gpu amf --size 64      # only AMF (plus CPU for comparison), 64 MiB
./file2video benchmark archive.zip              # real file, e.g. to check its MP4 size
```

The complete result table is always saved to the calibration file (default `file2video-calibration.conf` in the working directory), so the run doubles as machine calibration for [`--optimise`](#calibration-and-automatic-optimisation).

Backends whose trial encode fails (for example when a GPU driver is missing) are listed with their FFmpeg error. Run it before choosing `--chroma`/`--intra` to see what is fastest on your machine.

## Calibration and automatic optimisation

Every machine encodes differently: on a desktop with a strong GPU, AMF/NVENC/Quick Sync can be several times faster than the CPU; on other machines the all-intra CPU encoder wins on file size and decode speed. `benchmark` measures all of these combinations, stores the results in a small calibration file, and `--optimise` makes every encode automatically use the best one for *this* machine:

| Target | Meaning |
| --- | --- |
| `--optimise smallest` | Pick the measured configuration that produces the smallest MP4 for the payload. |
| `--optimise fastest` | Pick the measured configuration with the lowest encode + decode time. |
| `--optimise balanced` | Pick the best combination of speed and size (both normalised against the other candidates). |
| `--optimise off` | Default: always use exactly the flags and config values you passed. |

How it works:

- `benchmark` always writes `file2video-calibration.conf` in the working directory (change the location with `--calibration <PATH>`). It contains one measurement line per backend × mode (frames, payload, encode/decode seconds, MP4 size) plus `failed` lines for the combinations that do not work (for example a GPU driver that is missing).
- When you run `encode` with `--optimise` and no fresh calibration file exists yet, the tool first calibrates itself: it generates an 8 MiB pseudo-random payload, measures every backend × mode, saves the file, and continues with the winner.
- You do **not** have to benchmark again every time: the calibration file is reused as long as it is fresh. It records a **fingerprint** of your machine — OS and CPU architecture, CPU model, thread count, GPU names and driver versions, and the FFmpeg version. A hardware upgrade, a GPU driver update, or a different FFmpeg build changes the fingerprint, so the next `--optimise` encode automatically recalibrates once and saves the new results.
- With `--gpu <backend>` set, `--optimise` only chooses the frame mode (`--intra`/`--chroma`) within that backend; with `--gpu auto` (default) it may also choose between GPU backends and the CPU.
- Supported but overridden by explicit flags: `--chroma`/`--intra`, when passed, restrict the candidates to those that honour them.
- `estimate` shows which configuration `--optimise` would pick on this machine; it only reads the calibration file and never benchmarks.
- Encoding failures are retried once: when a configuration that `--optimise` selected turns out to fail (mid-encode FFmpeg errors do exist, see the AMF chroma note at [GPU acceleration](#gpu-acceleration)), the tool prints a warning and finishes the encode with your configured settings.

## Stealth mode

With `--stealth <VIDEO>`, `encode` does not store cells at all. Instead it hides the file as a faint, semi-transparent data layer over a background video you provide (for example gameplay footage). To a viewer the result looks like the original footage with a light digital-noise filter:

```sh
./file2video encode backup.zip hidden.mp4 --stealth gameplay.mp4 --password s3cret
./file2video decode hidden.mp4 backup.zip --password s3cret   # no gameplay.mp4 needed
```

How it works:

- The data layer only touches the **brightness (Y) channel**; the colours (U/V) of the background stay untouched. The frame is divided into rows of 6×6-pixel block pairs; each pair stores one bit by shifting the pair's mean luma towards one block or the other (±25/255, about 10% brightness). Base values are clamped to 30–225 and the shift is applied around the pair's mean, so the bit survives strong background gradients and re-encoding (tested through CRF 18, the default).
- A checkerboard anchor pattern along the frame edges lets the decoder align the grid even after scaling or a few dropped frames.
- The background video is looped (`-stream_loop -1`) for both picture and audio as long as data remains, and its audio is muxed into the output; `-shortest` ends the video with the last data frame.
- Decoding is **blind**: `decode` detects a stealth video automatically (it first tries the normal format, then falls back to the stealth decoder), aligns on the anchors, and reads each bit by comparing the mean luma of block A versus block B. You never need the background video again.
- Because re-encoded guerrilla data (streaming platforms use VP9/AV1) demands extra robustness, stealth defaults to 96 Reed-Solomon parity symbols per codeword (~38% redundancy) and CRF 18.
- Dimensions default to the background video's resolution rounded down to a multiple of 6 (the block size); `--width`, `--height`, `--fps`, `--crf`, `--preset`, `--ecc-symbols`, `--intra` and their config-file equivalents override the defaults as usual.

Limitations:

- An explicit `--chroma` (the colour planes carry the background colours) or an explicit `--optimise` (calibration profiles only cover the cell format) on the command line cannot be combined with `--stealth`; a `chroma = on` or `optimise = ...` default from the configuration file is ignored with a warning, so a config tuned for cell encodes never blocks a stealth encode.
- `estimate` and `benchmark` have no stealth support; use them in normal mode only.
- The 8-byte sync marker and the transport header are outside the Reed-Solomon protection. If the opening frames of the video are damaged so badly that anchors or header cannot be parsed, decoding stops; the file payload itself enjoys the full parity budget.
- Capacity per frame is much lower than the cell format (a 1920×1080 frame holds ~3.5 KB instead of ~130 KB), so the same file takes far more playback duration in stealth mode — this mode trades capacity for invisibility.

### `decode <INPUT> <OUTPUT>`

Recovers the original file from the video. The resolution, block size, cell depth and whether data was stored in the color planes are detected automatically. The [stealth format](#stealth-mode) is also detected automatically: when the regular decoder rejects the first frame, the blind stealth decoder tries next.

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

Prints the exact encrypted size, frame count and playback duration for a file, without writing any video. It accepts the same video options as `encode`, plus `--optimise`: with a fresh calibration it also prints which backend and frame mode an optimised encode would use on this machine. It never benchmarks and cannot predict the MP4 file size or the encoding time.

### Options for all commands

| Option | Description |
| --- | --- |
| `--ffmpeg <PATH>` | FFmpeg executable (default: from `PATH`, auto-downloaded when missing). |
| `--no-download` | Never download FFmpeg; use what is already on the system. |
| `--ffprobe <PATH>` | ffprobe executable (default: next to FFmpeg, else `PATH`). |
| `--gpu <MODE>` | Hardware encoder for `encode`: `auto` (default), `off`, `amf`, `nvenc`, `qsv`, `vaapi`, `video-toolbox`. See [GPU acceleration](#gpu-acceleration). |
| `--threads <N>` | Thread limit for the Rust error-correction pool and for FFmpeg (default: `0` = automatic). |
| `--config <PATH>` | Configuration file with extra defaults; default: `file2video.conf` in the working directory when it exists. See [Configuration file](#configuration-file). |
| `--calibration <PATH>` | Calibration database; default `file2video-calibration.conf` in the working directory. See [Calibration and automatic optimisation](#calibration-and-automatic-optimisation). |

## Configuration file

Most settings become defaults in an optional configuration file instead of long command lines. `file2video` automatically loads `file2video.conf` from the working directory when it exists; `--config <PATH>` loads an explicit file instead (a missing explicit file is an error; a missing default file is fine).

Values in the file are defaults: every command-line flag you pass overrides it. Unknown values are rejected with the file name and line number, so mistakes are easy to find.

Don't want to edit text files? The [config generator](https://vulcanosoftware.github.io/file2video/) builds this file in your browser — pick your values, copy the result, done. It is a static page served from [docs/index.html](docs/index.html).

Maintainers: the page deploys automatically on every push to `main` that touches `docs/` ([pages.yml](.github/workflows/pages.yml)). GitHub does not let the workflow's own token create a Pages site, so before the first deploy, enable Pages once: **Settings → Pages → Build and deployment → Source: GitHub Actions**, then re-run the workflow from the Actions tab.

| Key | Values (default) | Description |
| --- | --- | --- |
| `width`, `height` | even numbers ≤ 8192 (1920, 1080) | Storage grid. |
| `block_size` | `8`, `10`, `12`, `16`, `20` (8) | Pixels per data cell. |
| `fps` | 1–60 (30) | Playback speed. |
| `compress` | `light`, `medium`, `high` | Compression profile (sets preset + parity like `--compress`). |
| `crf` | 0–51 (8) | x264 quality. |
| `preset` | `ultrafast` … `veryslow` (`faster`) | Speed/size preset. |
| `chroma` | `on`/`off` (off) | Also store data in the color planes. Accepts `on`/`yes`/`true`/`1` and `off`/`no`/`false`/`0`. |
| `intra` | `on`/`off` (off) | All-intra frames. |
| `ecc_symbols` | 1–127 (32, or set by `compress`) | Reed-Solomon parity per codeword. |
| `pbkdf2_iterations` | 1–10,000,000 (600000) | PBKDF2-HMAC-SHA256 work factor. |
| `threads` | 0–256 (0) | Thread limit; 0 = automatic. |
| `gpu` | `auto`, `off`, `amf`, `nvenc`, `qsv`, `vaapi`, `video-toolbox` (auto) | Hardware encoder choice. |
| `optimise` | `off`, `smallest`, `fastest`, `balanced` (off) | Use the saved calibration, see above. |

Example:

```ini
# file2video.conf
optimise = balanced
fps = 30
ecc_symbols = 32
gpu = auto
pbkdf2_iterations = 600000
# chroma = on
# intra = off
```

The calibration database has its own file, `file2video-calibration.conf`, also in the working directory by default; move it with `--calibration <PATH>`. Neither file contains secrets.

*Note on encryption:* encoding is password-protected by default (AES-256-GCM + PBKDF2) because the videos are designed to travel through YouTube and similar platforms. Pass `--no-password` to store the file unencrypted (SHA-256 integrity check only); there is deliberately no `password =` value in the configuration file, so passwords never end up on disk by accident. `pbkdf2_iterations` remains the tuning knob for the work factor. Everything else in the configuration file is optional.

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

If FFmpeg dies in the middle of an encode instead of cleanly failing (for example a broken-pipe message), the error now carries the last lines of FFmpeg's own stderr (`FFmpeg reported: …`), so the real cause is visible without rerunning with `FILE2VIDEO_DEBUG=1`.

Set `FILE2VIDEO_DEBUG=1` to also print the exact FFmpeg command used for each GPU trial encode, which is useful when reporting a problem.

## Passwords

`encode` asks for a password and a confirmation; `decode` asks once. For scripts, use `--password-file <FILE>`: a UTF-8 text file of at most 64 KiB, of which one trailing newline is ignored. `--password <TEXT>` also works but can leak through shell history and process lists. The two options cannot be combined, and an empty password is rejected.

Encryption is the default, but encoding stores the file in the clear when you pass `--no-password`: the video then keeps only a SHA-256 integrity check, so a damaged video is still detected, while anyone who downloads it can read the file. Videos encoded with `--no-password` never ask for a password on decode (passing one is harmless), and the decoder detects automatically whether a video needs one.

Passwords can only contain Latin-1 characters (for example ASCII and `é`). Other characters are rejected.

## Size, duration and disk space

- **Capacity:** each pixel of the video stores one cell with 4 bits (one of 16 brightness levels), so one frame carries 32,768 bytes at the standard 256×256 cell grid, of which about 28,600 are file data after error correction. That is roughly 860 KB per second of video. 1 MiB takes 37 frames (1.2 s); 1 GiB takes about 37,500 frames (21 minutes). With `--chroma` the U and V planes carry data too: +50% per frame (1.29 MB per second), so about a third fewer frames and a third faster in both directions at roughly the same MP4 size.
- **MP4 size:** the video is stored at cell resolution (256×256 or larger, one cell per pixel) and only upscaled during playback for viewing, so the MP4 tracks the payload size. Compact mode encodes at capped quality (`--crf` is capped at 8, or 6 with `--intra`) because every cell must survive H.264 quantization intact; for random or compressed data — which H.264 cannot compress — expect roughly 1.7× the input (about 1.6× with `--intra`). Use `estimate` for the duration, not the file size; use `benchmark` to measure speed and size on your machine.
- **File size limit:** a single file must be just under 64 GiB (the AES-GCM limit).
- **Disk space:** `encode` stages the ciphertext in a temporary file in the output directory, because the format stores the authentication tag before the ciphertext. Budget about the input size for that, plus the MP4 itself. `decode` stages the restored file in the output directory before publishing it.
- **Speed:** encoding uses a hardware GPU encoder when one is available (see [GPU acceleration](#gpu-acceleration)); otherwise x264 at the default `faster` preset dominates encoding time. `--preset veryfast` is even faster at the cost of a different size/quality trade-off; verify the result with a round trip. Error correction runs in parallel and uses AVX2 on x86-64 when available, with a portable fallback.

## GPU acceleration

`encode` uses the hardware H.264 encoder on your GPU by default. A small trial encode checks each candidate in order and the first working one is used; if none works, a notice is printed and the video is encoded on the CPU with `libx264` as before.

| System | Backends tried in order | Values for `--gpu` |
| --- | --- | --- |
| Windows | AMF (AMD), NVENC (NVIDIA), Quick Sync (Intel) | `amf`, `nvenc`, `qsv` |
| Linux | VA-API (`/dev/dri/renderD128`), NVENC, Quick Sync | `vaapi`, `nvenc`, `qsv` |
| macOS | VideoToolbox | `video-toolbox` |

- Hybrid laptops: when a discrete GPU is detected (any NVIDIA card, Intel Arc, or an AMD Radeon RX model), its encoder is tried before the integrated GPU's.
- With `--optimise`, the measured calibration decides which backend wins on this machine instead of the trial order above; see [Calibration and automatic optimisation](#calibration-and-automatic-optimisation). Some combinations are broken in specific GPU drivers (for example the chroma modes of the AMF encoder on certain Adrenalin driver versions, which fail mid-encode); the benchmark marks those combinations as failed and `--optimise` never selects them.
- A GPU is never required: without a working hardware encoder, or with `--gpu off`, the video is encoded on the CPU with `libx264`. FFmpeg still needs `libx264` for that fallback path.
- A specific `--gpu` value forces that backend and fails if it is not available in your FFmpeg build or its trial encode fails.
- The GPU encoder is used for encoding only; decoding always runs on the CPU.
- `--crf` and `--preset` are translated per backend: constant QP on AMF and VA-API, `-cq` on NVENC, ICQ quality on Quick Sync, and a quality setting on VideoToolbox. The presets map to each encoder's speed levels.
- Verify the result with a round trip (`encode`, then `decode`, then compare hashes), especially when you use non-default `--crf`, `--preset`, `--chroma`, `--intra` or `--compress` settings. Use `benchmark` to check which backend and modes actually work and how fast they are on your machine.
- Troubleshooting: when every backend fails, each one prints its FFmpeg trial error (for example `AMF_NO_DLL` means the AMD Adrenalin driver is missing or too old). Update the GPU driver and run again; use `--gpu off` to encode on the CPU without diagnostics.

## Reliability and security

- **Error correction:** Reed-Solomon (255 bytes per codeword) repairs up to half the parity count in corrupted bytes per codeword, which is 32 bytes at the default of 64. It repairs damaged pixels, not missing frames.
- **The video is not a backup:** cropping, scaling, trimming, dropped frames or heavy re-encoding (for example by a video platform) can make recovery impossible. Keep the original file.
- **Encryption (default, optional with `--no-password`):** AES-256-GCM with a key from PBKDF2-HMAC-SHA256 (600,000 iterations by default) and a random salt and nonce per file. The original filename is encrypted along with the data. A wrong password or a damaged video fails authentication, and no output is written.
- **Without a password (`--no-password`):** anyone who downloads the video can read the file byte for byte; the stored SHA-256 checks integrity only, so decoding still refuses damaged videos but offers no secrecy.
- **What is not hidden:** the video visibly shows cells with 16 grey brightness levels, and its length reveals the approximate file size.

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

CI builds and tests Windows x64, Linux x64/ARM64 and macOS x64/ARM64.

## Releases

- Every push to `main` that passes all platform builds publishes a release tagged `build-<commit>` with all five archives and their SHA-256 files. A rerun never creates a duplicate.
- Pushing a tag `vX.Y.Z` that matches the version in `Cargo.toml` publishes a numbered release.
- Pull request builds are only available as temporary [Actions artifacts](https://github.com/VulcanoSoftware/file2video/actions/workflows/ci.yml).
