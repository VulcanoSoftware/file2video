# file2video

A Rust command-line tool that turns a file into an encrypted H.264 MP4 and restores the file byte for byte. The MP4 format is compatible in both directions with [Bit2Vid v1](https://github.com/nuan-cmyk/Bit2Vid). No Python or Rust installation is required to run a prebuilt binary.

## Install

Download the archive for your OS and CPU from [GitHub Releases](https://github.com/vectrastack/file2video/releases) and extract it. Each successful build on `main` publishes a release with all five platform archives and their SHA-256 checksums. Pull request builds are only available as temporary [Actions artifacts](https://github.com/vectrastack/file2video/actions/workflows/ci.yml).

Install [FFmpeg](https://ffmpeg.org/download.html) with `libx264` and `ffprobe` as well:

| Platform | FFmpeg installation |
| --- | --- |
| Windows x64 | `winget install --id Gyan.FFmpeg -e`, then reopen PowerShell |
| Ubuntu / Debian | `sudo apt install ffmpeg` |
| macOS (Intel or Apple Silicon) | `brew install ffmpeg` |

Check `ffmpeg -version` and `ffprobe -version` before running the CLI. If needed, supply `--ffmpeg /path/to/ffmpeg`; the tool looks for `ffprobe` next to that executable unless `--ffprobe` is set. GitHub builds Linux x64 on Ubuntu 22.04 and Linux ARM64 on Ubuntu 24.04; older systems may need to build from source.

## Use

In PowerShell, from the extracted directory:

```powershell
.\file2video.exe estimate .\1GB.bin
.\file2video.exe encode .\1GB.bin .\output.mp4
.\file2video.exe decode .\output.mp4 .\restored.bin
Get-FileHash .\1GB.bin, .\restored.bin
```

On Linux or macOS:

```sh
./file2video estimate archive.zip
./file2video encode archive.zip archive.mp4
mkdir recovered
./file2video decode archive.mp4 recovered
```

`encode` prompts for a password and asks you to confirm it; `decode` prompts once. Bit2Vid v1 passwords use Latin-1 for interoperability (for example, ASCII and `é`). To automate, use `--password-file` with a protected text file. `--password` exists for compatibility, but can expose the password in your history or a process listing.

Decode detects the resolution and block size automatically. An existing output directory restores the stored name, and an explicit file path lets you choose the name. The output directory must already exist; existing files are never overwritten. Decode publishes an output only after verifying the AES-GCM tag and SHA-256, and deletes ordinary temporary outputs on failure.

## Quality and performance

The defaults are unchanged from Bit2Vid: 1920×1080, 30 FPS, 8-pixel blocks, x264 `slow` at CRF 18, 64 parity symbols per 255-byte Reed-Solomon codeword, and AES-256-GCM with PBKDF2-HMAC-SHA256 at 600,000 iterations. That ECC can repair up to 32 changed bytes per codeword, not 32 dropped frames. The video is not a substitute for the original backup: cropping, scaling, trimming, frame dropping or heavy transcoding may make recovery impossible.

- Rust computes compatible parity in reusable parallel batches. It uses AVX2 when available, with a portable fallback. On clean decode, it checks parity before invoking the slower error-correction decoder.
- Encode streams the compact bit grid to FFmpeg for integer nearest-neighbor expansion, rather than sending fully expanded frames. Rust keeps a bounded set of working buffers and uses temporary disk space for the ciphertext, because Bit2Vid places the authentication tag before the ciphertext. Budget for roughly the input file size in scratch space, in addition to the MP4, in the output directory. Decode stages the restored file there before publishing it.
- The same CPU video encoder remains the default. No GPU is needed. Hardware encoders are not offered here because they do not reproduce the x264 quality/size trade-off and have not been validated for equivalent recovery. If you are willing to trade output size or compression behavior for speed, try `--preset veryfast` and verify the output with a round trip. `--compress` profiles can also change ECC and recovery margin; leave them unset to keep the original defaults.
- Large files still produce long videos: use `estimate` to check exact frame count and playback duration first. It does not predict MP4 size or encode time. For Bit2Vid v1 compatibility a single encrypted message must be smaller than 64 GiB.

### Example benchmark

On a two-vCPU Linux machine with FFmpeg 8, using the same 256 KiB input and default settings, Bit2Vid Python took 8.42 s to encode and 3.34 s to decode; file2video took 6.02 s and 0.68 s. Both restored exactly the same bytes and produced MP4s of about 944 kB. These are single-run timings, not estimates for a 1 GiB encode. In a separate parity-only test on that machine, the Rust path produced about 1.34 million RS(255,191) codewords/s (128 MiB input), versus about 1,350 codewords/s for Bit2Vid's two-worker Python path (1 MiB input). The parity benchmark excludes H.264; x264 remains the main encode cost.

## Develop or release

From a checkout with the Rust toolchain and FFmpeg installed:

```sh
cargo build --release --locked
cargo test --release --locked
cargo clippy --locked --all-targets -- -D warnings
cargo test --release --locked ecc_throughput -- --ignored --nocapture
```

CI builds and tests the CLI for Windows x64, Linux x64/ARM64 and macOS x64/ARM64. Both macOS jobs use Apple Silicon runners; the Intel binary is cross-compiled and tested through Rosetta, which is only needed for CI, not for running the binary on an Intel Mac. Linux CI also checks Python → Rust and Rust → Python against a pinned Bit2Vid checkout.

Pushes and merges to `main` publish automatically after all platform builds pass. The release is tied to the exact tested commit using an automatically created `build-<commit>` tag. Rerunning a build does not create a duplicate or replace an existing release. To publish a separate numbered release, push a `vX.Y.Z` tag matching the version in `Cargo.toml`.
