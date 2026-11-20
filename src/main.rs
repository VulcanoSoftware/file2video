mod crypto;
mod diag;
mod download;
mod ecc;
mod format;
mod video;

use anyhow::{Context, Result, bail, ensure};
use clap::{Args, Parser, Subcommand, ValueEnum};
use format::{CRYPTO_SIZE, ECC_SIZE, Grid, Layout, MAX_KDF, TRANSPORT_SIZE, video_dimension};
use std::fs::{self, File};
use std::io::{Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;
use tempfile::NamedTempFile;
use zeroize::Zeroizing;

const CHUNK: usize = 256 * 1024;
const PRESETS: [&str; 9] = [
    "ultrafast",
    "superfast",
    "veryfast",
    "faster",
    "fast",
    "medium",
    "slow",
    "slower",
    "veryslow",
];

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Operation,
    /// FFmpeg executable (default: PATH).
    #[arg(long, global = true, value_name = "PATH")]
    ffmpeg: Option<PathBuf>,
    /// ffprobe executable (default: next to FFmpeg or on PATH).
    #[arg(long, global = true, value_name = "PATH")]
    ffprobe: Option<PathBuf>,
    /// Thread limit for each of the Rust ECC pool and FFmpeg (0 = automatic).
    #[arg(long, global = true, default_value_t = 0)]
    threads: usize,
    /// Hardware encoder for the video (auto/off/amf/nvenc/qsv/vaapi/video-toolbox).
    #[arg(long, global = true, value_enum, default_value_t = video::Gpu::Auto)]
    gpu: video::Gpu,
    /// Do not download a full FFmpeg build automatically (default: download when missing).
    #[arg(long, global = true)]
    no_download: bool,
}

#[derive(Subcommand)]
enum Operation {
    /// Turn a file into an encrypted and error-corrected H.264/MP4 video.
    Encode {
        input: PathBuf,
        output: PathBuf,
        #[command(flatten)]
        password: Password,
        #[command(flatten)]
        settings: Encoding,
    },
    /// Recover and authenticate an original file from an MP4 video.
    Decode {
        input: PathBuf,
        output: PathBuf,
        #[command(flatten)]
        password: Password,
        /// Restore the original filename in the specified directory (or the output's parent).
        #[arg(long)]
        restore_name: bool,
        /// Override resolution detection (requires --height).
        #[arg(long, requires = "height")]
        width: Option<usize>,
        /// Override resolution detection (requires --width).
        #[arg(long, requires = "width")]
        height: Option<usize>,
        /// Override block-size detection.
        #[arg(long)]
        block_size: Option<usize>,
    },
    /// Calculate the exact frame count and playback duration without writing video.
    Estimate {
        input: PathBuf,
        #[command(flatten)]
        settings: Encoding,
    },
}

#[derive(Args)]
struct Password {
    /// Read the password from a UTF-8 text file (one trailing newline is ignored).
    #[arg(long, conflicts_with = "password")]
    password_file: Option<PathBuf>,
    /// Set the password directly (may be visible in shell history and process lists).
    #[arg(long, hide_default_value = true)]
    password: Option<String>,
}

impl Password {
    fn get(self, confirm: bool) -> Result<Zeroizing<Vec<u8>>> {
        let value = Zeroizing::new(if let Some(s) = self.password {
            s
        } else if let Some(path) = self.password_file {
            const LIMIT: u64 = 65536;
            let mut text = String::new();
            File::open(&path)?
                .take(LIMIT + 1)
                .read_to_string(&mut text)?;
            ensure!(text.len() as u64 <= LIMIT, "password file is too large");
            if text.ends_with('\n') {
                text.pop();
                if text.ends_with('\r') {
                    text.pop();
                }
            }
            text
        } else {
            let typed = Zeroizing::new(rpassword::prompt_password("Password: ")?);
            if confirm {
                let repeated = Zeroizing::new(rpassword::prompt_password("Confirm password: ")?);
                ensure!(*typed == *repeated, "passwords do not match");
            }
            return crypto::password_bytes(&typed);
        });
        crypto::password_bytes(&value)
    }
}

#[derive(Clone, ValueEnum)]
enum Profile {
    Light,
    Medium,
    High,
}

#[derive(Args)]
struct Encoding {
    #[arg(long, default_value_t = 1920)]
    width: usize,
    #[arg(long, default_value_t = 1080)]
    height: usize,
    #[arg(long, default_value_t = 8)]
    block_size: usize,
    #[arg(long, default_value_t = 30, value_parser = clap::value_parser!(u8).range(1..=60))]
    fps: u8,
    /// Use a smaller-output profile (can reduce recovery margin).
    #[arg(long, value_enum)]
    compress: Option<Profile>,
    /// x264 quality (default: 8; capped at 8 so every cell survives quantization).
    #[arg(long, value_parser = clap::value_parser!(u8).range(0..=51))]
    crf: Option<u8>,
    /// x264 speed/size preset (default: faster, or set by --compress, for the CPU fallback and GPU speed tiers).
    #[arg(long, value_parser = clap::builder::PossibleValuesParser::new(PRESETS))]
    preset: Option<String>,
    /// Reed-Solomon parity bytes per 255-byte codeword (default: 32, or set by --compress).
    #[arg(long, value_parser = clap::value_parser!(u8).range(1..=127))]
    ecc_symbols: Option<u8>,
    /// Number of PBKDF2-HMAC-SHA256 iterations.
    #[arg(long, default_value_t = 600_000, value_parser = clap::value_parser!(u32).range(1..=i64::from(MAX_KDF)))]
    pbkdf2_iterations: u32,
}

impl Encoding {
    fn resolve(self) -> Result<(video::Settings, usize, u32)> {
        let display = Grid {
            width: self.width,
            height: self.height,
            block: self.block_size,
        }
        .validate()?;
        // Compact mode stores one cell per video pixel at cell resolution;
        // the padded cells carry stream data just like any other cell.
        let (crf, preset, parity) = match self.compress {
            None => (8, "faster", 32),
            Some(Profile::Light) => (8, "slow", 64),
            Some(Profile::Medium) => (8, "slower", 32),
            Some(Profile::High) => (8, "veryslow", 16),
        };
        let grid = Grid {
            width: video_dimension(display.width / display.block),
            height: video_dimension(display.height / display.block),
            block: 1,
        };
        Ok((
            video::Settings {
                grid,
                display,
                fps: self.fps,
                crf: self.crf.unwrap_or(crf),
                preset: self.preset.unwrap_or_else(|| preset.to_owned()),
                cell_bits: 4,
                compact: true,
            },
            self.ecc_symbols.map_or(parity, usize::from),
            self.pbkdf2_iterations,
        ))
    }
}

fn parent(path: &Path) -> &Path {
    path.parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

fn temporary(dir: &Path) -> Result<NamedTempFile> {
    tempfile::Builder::new()
        .prefix(".file2video-")
        .tempfile_in(dir)
        .with_context(|| format!("could not create a temporary file in {}", dir.display()))
}

fn check_output(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(_) => bail!("output already exists: {}", path.display()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

fn publish(file: NamedTempFile, path: &Path) -> Result<()> {
    file.persist_noclobber(path)
        .with_context(|| format!("could not publish {}", path.display()))?;
    Ok(())
}

fn regular_file_len(meta: fs::Metadata) -> Result<u64> {
    ensure!(meta.is_file(), "input must be a regular file");
    Ok(meta.len())
}

fn encode(
    tools: &video::Tools,
    input: &Path,
    output: &Path,
    password: Password,
    options: Encoding,
) -> Result<()> {
    let (settings, parity, iterations) = options.resolve()?;
    check_output(output)?;
    regular_file_len(fs::metadata(input)?)?;
    let file = File::open(input)?;
    let size = regular_file_len(file.metadata()?)?;
    let metadata = format::metadata(input, size)?;
    let plain_size = size
        .checked_add(metadata.len() as u64)
        .context("input size overflow")?;
    let mut header = crypto::Header::new(iterations, plain_size)?;
    let encrypted_size = plain_size + CRYPTO_SIZE as u64;
    let layout = if settings.compact {
        Layout::compact(encrypted_size, parity)?
    } else {
        Layout::new(encrypted_size, parity)?
    };
    let password = password.get(true)?;
    let mut crypto = crypto::Stream::new(&header, &password);
    drop(password);
    let frames = layout.frames(settings.grid, settings.cell_bits);
    eprintln!(
        "Encrypting {size} bytes; writing {frames} video frames ({} luma levels per cell).",
        1 << settings.cell_bits
    );

    // The format places the final GCM tag before the ciphertext. Stage only
    // the ciphertext (never the input file) so the original is read once.
    let dir = parent(output);
    let mut staged = tempfile::tempfile_in(dir).with_context(|| {
        format!(
            "could not create ciphertext scratch file in {}",
            dir.display()
        )
    })?;
    let mut source = metadata.as_slice().chain(file);
    let mut buffer = Zeroizing::new(vec![0; CHUNK]);
    let mut left = plain_size;
    while left != 0 {
        let n = left.min(CHUNK as u64) as usize;
        let chunk = &mut buffer[..n];
        source
            .read_exact(chunk)
            .context("could not read input file (it may have shrunk)")?;
        crypto.update(chunk, true)?;
        staged
            .write_all(chunk)
            .context("could not stage ciphertext (check free disk space)")?;
        left -= n as u64;
    }
    ensure!(
        source.read(&mut [0])? == 0,
        "input file grew while encoding"
    );
    (header.tag, header.hash) = crypto.finish()?;
    staged.rewind()?;
    let bytes = header.bytes();
    let encrypted = bytes.as_slice().chain(staged);

    let tmp = temporary(dir)?;
    let mut process = tools.encode(&settings, tmp.path())?;
    let pipe = process.child.stdin.take().context("missing FFmpeg stdin")?;
    let mut sink = video::Sink::new(pipe, settings.grid, frames);
    sink.push(&format::transport(
        settings.display,
        parity,
        layout.protected,
        settings.cell_bits,
        settings.compact,
    ))?;
    let codec = ecc::Codec::new(parity);
    if settings.compact {
        // In compact mode the ECC header leads the first Reed-Solomon word's
        // data, so parity protects it against quantization noise too.
        let ecc_header = layout.ecc_header(parity);
        codec.encode(
            ecc_header.as_slice().chain(encrypted),
            ECC_SIZE as u64 + layout.encrypted,
            |data| sink.push(data),
        )?;
    } else {
        sink.push(&layout.ecc_header(parity))?;
        codec.encode(encrypted, layout.encrypted, |data| sink.push(data))?;
    }
    sink.finish()?;
    process.finish()?;
    publish(tmp, output)
}

fn restorable_name(name: &str) -> Result<&str> {
    ensure!(
        name != "."
            && name != ".."
            && !name.ends_with(['.', ' '])
            && !name.contains(['/', '\\', ':', '<', '>', '"', '|', '?', '*'])
            && !name.chars().any(char::is_control),
        "stored filename is not safe to restore; use an explicit output filename"
    );
    let stem = name
        .split('.')
        .next()
        .unwrap()
        .trim_end()
        .to_ascii_uppercase();
    let numbered_device = ["COM", "LPT"].into_iter().any(|p| {
        stem.strip_prefix(p).is_some_and(|n| {
            matches!(
                n,
                "0" | "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
            )
        })
    });
    ensure!(
        !numbered_device
            && !matches!(
                stem.as_str(),
                "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
            ),
        "stored filename is a device name; use an explicit output filename"
    );
    Ok(name)
}

struct DecodeOptions {
    restore_name: bool,
    width: Option<usize>,
    height: Option<usize>,
    block: Option<usize>,
}

fn decode(
    tools: &video::Tools,
    input: &Path,
    output: &Path,
    password: Password,
    options: DecodeOptions,
) -> Result<PathBuf> {
    regular_file_len(fs::metadata(input)?)?;
    let is_dir = output.is_dir();
    if !is_dir && !options.restore_name {
        check_output(output)?;
    }
    let dir = if is_dir { output } else { parent(output) };
    let (width, height) = if let Some((width, height)) = options.width.zip(options.height) {
        format::validate_resolution(width, height)?;
        (width, height)
    } else {
        tools.dimensions(input)?
    };
    let mut process = tools.decode(input)?;
    let pipe = process
        .child
        .stdout
        .take()
        .context("missing FFmpeg stdout")?;
    let (mut frames, parity, protected) = video::Source::open(pipe, width, height, options.block)?;
    let grid = frames.grid();
    let cell_bits = frames.cell_bits();
    let codec = ecc::Codec::new(parity);
    let mut ecc_header = [0; ECC_SIZE];
    let (mut reader, layout) = if frames.compact() {
        // Plain allowance for the codewords: words carry 255 - parity data
        // bytes each, which is all the encoder ever wrote.
        let words = (protected - ECC_SIZE as u64) / 255;
        let mut stream = codec.reader(&mut frames, words * (255 - parity) as u64);
        stream.read_exact(&mut ecc_header)?;
        let layout = Layout::parse(&ecc_header, parity, protected, true)?;
        (stream, layout)
    } else {
        frames.read_exact(&mut ecc_header)?;
        let layout = Layout::parse(&ecc_header, parity, protected, false)?;
        (codec.reader(&mut frames, layout.encrypted), layout)
    };
    reader
        .input_mut()
        .set_frames(layout.frames(grid, cell_bits));
    let mut header = [0; CRYPTO_SIZE];
    reader.read_exact(&mut header)?;
    let header = crypto::Header::parse(&header, layout.encrypted - CRYPTO_SIZE as u64)?;
    let password = password.get(false)?;
    let mut crypto = crypto::Stream::new(&header, &password);
    drop(password);
    let mut tmp = temporary(dir)?;
    let mut buffer = Zeroizing::new(vec![0; CHUNK]);

    let n = header.size.min(CHUNK as u64) as usize;
    reader.read_exact(&mut buffer[..n])?;
    crypto.update(&mut buffer[..n], false)?;
    // Do not trust metadata until authentication has succeeded.
    let metadata = format::parse_metadata(&buffer[..n], header.size)
        .map(|(name, offset)| (name.map(str::to_owned), offset));
    let offset = metadata.as_ref().map(|(_, offset)| *offset).unwrap_or(0);
    tmp.write_all(&buffer[offset..n])?;
    let mut left = header.size - n as u64;
    while left != 0 {
        let n = left.min(CHUNK as u64) as usize;
        let chunk = &mut buffer[..n];
        reader.read_exact(chunk)?;
        crypto.update(chunk, false)?;
        tmp.write_all(chunk)?;
        left -= n as u64;
    }
    crypto.verify(&header)?;
    drop(reader);
    frames.finish()?;
    process.finish()?;
    let (name, _) = metadata?;
    let destination = match (name, is_dir, options.restore_name) {
        (Some(name), true, _) | (Some(name), _, true) => dir.join(restorable_name(&name)?),
        (None, true, _) => bail!("video has no stored filename; specify an output filename"),
        _ => output.to_owned(),
    };
    publish(tmp, &destination)?;
    Ok(destination)
}

fn estimate(input: &Path, options: Encoding) -> Result<()> {
    let (settings, parity, _) = options.resolve()?;
    let size = regular_file_len(fs::metadata(input)?)?;
    let metadata = format::metadata(input, size)?.len() as u64;
    let encrypted = size
        .checked_add(metadata + CRYPTO_SIZE as u64)
        .context("input size overflow")?;
    let layout = if settings.compact {
        Layout::compact(encrypted, parity)?
    } else {
        Layout::new(encrypted, parity)?
    };
    let frames = layout.frames(settings.grid, settings.cell_bits);
    println!("Input:            {size} bytes");
    println!("Encrypted:        {} bytes", layout.encrypted);
    println!(
        "Video payload:    {} bytes including Reed-Solomon and transport headers",
        layout.protected + TRANSPORT_SIZE as u64
    );
    println!(
        "Grid:             {}x{} pixels, {} pixels/block",
        settings.display.width, settings.display.height, settings.display.block
    );
    if settings.compact {
        println!(
            "Video:            {}x{} pixels, one cell per pixel",
            settings.grid.width, settings.grid.height
        );
    }
    println!(
        "Cell depth:       {} luma levels per cell",
        1 << settings.cell_bits
    );
    println!("Frames:           {frames}");
    println!(
        "Duration:         {:.2} seconds at {} fps",
        frames as f64 / f64::from(settings.fps),
        settings.fps
    );
    println!("MP4 file size depends on H.264 compression and cannot be predicted exactly.");
    Ok(())
}

// The estimate command never touches FFmpeg; encode and decode do.
fn uses_video(cli: &Cli) -> bool {
    !matches!(cli.command, Operation::Estimate { .. })
}

fn pick_ffmpeg(cli: &Cli) -> PathBuf {
    if let Some(path) = &cli.ffmpeg {
        return path.clone();
    }
    let default = PathBuf::from(video::default_ffmpeg());
    if cli.no_download || !uses_video(cli) {
        return default;
    }
    let full_build = download::ffmpeg_works(&default)
        && (cli.gpu == video::Gpu::Off || download::has_hardware_encoders(&default));
    if full_build {
        return default;
    }
    match download::install() {
        Ok(path) => path,
        Err(error) => {
            eprintln!("Warning: {error:#}. Continuing with {}.", default.display());
            default
        }
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    ensure!(cli.threads <= 256, "--threads must be in 0..=256");
    let with_video = uses_video(&cli);
    let tools = video::Tools {
        ffmpeg: pick_ffmpeg(&cli),
        ffprobe: cli.ffprobe,
        threads: cli.threads,
        gpu: cli.gpu,
    };
    if cli.threads != 0 {
        rayon::ThreadPoolBuilder::new()
            .num_threads(cli.threads)
            .build_global()?;
    }
    if with_video {
        diag::print(&tools.ffmpeg);
    }
    let start = Instant::now();
    match cli.command {
        Operation::Encode {
            input,
            output,
            password,
            settings,
        } => {
            encode(&tools, &input, &output, password, settings)?;
            eprintln!(
                "Wrote {} in {:.2}s.",
                output.display(),
                start.elapsed().as_secs_f64()
            );
        }
        Operation::Decode {
            input,
            output,
            password,
            restore_name,
            width,
            height,
            block_size,
        } => {
            let done = decode(
                &tools,
                &input,
                &output,
                password,
                DecodeOptions {
                    restore_name,
                    width,
                    height,
                    block: block_size,
                },
            )?;
            eprintln!(
                "Restored {} in {:.2}s.",
                done.display(),
                start.elapsed().as_secs_f64()
            );
        }
        Operation::Estimate { input, settings } => estimate(&input, settings)?,
    }
    Ok(())
}

fn main() -> std::process::ExitCode {
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("Error: {error:#}");
            std::process::ExitCode::FAILURE
        }
    }
}
