mod calibrate;
mod config;
mod crypto;
mod diag;
mod download;
mod ecc;
mod format;
mod stealth;
mod video;

use calibrate::{Calibration, Target};
use config::{Config, Profile};
use video::speed;

use anyhow::{Context, Result, bail, ensure};
use clap::{Args, Parser, Subcommand};
use format::{CRYPTO_SIZE, ECC_SIZE, Grid, Layout, MAX_KDF, TRANSPORT_SIZE, video_dimension};
use std::fs::{self, File};
use std::io::{BufReader, Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;
use tempfile::NamedTempFile;
use zeroize::Zeroizing;

const CHUNK: usize = 256 * 1024;
const CALIBRATION_PAYLOAD_MIB: u64 = 8;
const MODES: [(&str, bool, bool); 4] = [
    ("base", false, false),
    ("intra", true, false),
    ("chroma", false, true),
    ("intra+chroma", true, true),
];

/// Slices of shared context every operation receives from `run`.
struct Ctx<'a> {
    config: &'a Config,
    calibration: &'a Path,
    quiet: bool,
}

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
    /// Thread limit for each of the Rust ECC pool and FFmpeg (default: 0 = automatic).
    #[arg(long, global = true, value_name = "N")]
    threads: Option<usize>,
    /// Hardware encoder for the video (auto/off/amf/nvenc/qsv/vaapi/video-toolbox).
    #[arg(long, global = true, value_enum, value_name = "NAME")]
    gpu: Option<video::Gpu>,
    /// Configuration file with extra defaults (default: file2video.conf in the working directory).
    #[arg(long, global = true, value_name = "PATH")]
    config: Option<PathBuf>,
    /// Calibration database (default: file2video-calibration.conf in the working directory).
    #[arg(long, global = true, value_name = "PATH")]
    calibration: Option<PathBuf>,
    /// Do not download a full FFmpeg build automatically (default: download when missing).
    #[arg(long, global = true)]
    no_download: bool,
}

#[derive(Subcommand)]
enum Operation {
    /// Turn a file into an error-corrected H.264/MP4 video
    /// (password-encrypted by default; see --no-password).
    Encode {
        input: PathBuf,
        output: PathBuf,
        /// Store the file without a password: skip encryption and keep only
        /// a SHA-256 integrity check (anyone who downloads the video can
        /// read the file).
        #[arg(long, conflicts_with_all = ["password", "password_file"])]
        no_password: bool,
        #[command(flatten)]
        password: Password,
        #[command(flatten)]
        settings: Encoding,
        /// Hide the data as a faint differential-luma layer over this
        /// background video; decoding is blind and does not need it again
        /// (see the README, "Stealth mode").
        #[arg(long, value_name = "VIDEO_PATH")]
        stealth: Option<PathBuf>,
        /// Stealth modulation block size in pixels (even, 2-16, default 4).
        /// Each bit costs two blocks of pixels, so smaller blocks carry more
        /// data per frame and encode proportionally faster: block 2 gives
        /// about 4x the default throughput. Larger blocks (6, default 4
        /// alternative) trade throughput for robustness to heavy
        /// re-encoding such as YouTube.
        #[arg(long, value_name = "PIXELS", value_parser = clap::value_parser!(u16).range(2..=16))]
        stealth_block: Option<u16>,
        /// Stealth data layer visibility (luma levels, 5-50, default 25): the
        /// outer modulation levels sit at ±this value. Higher values make the
        /// layer easier to see but survive aggressive re-encoding (YouTube)
        /// better; for dense mode use 40-50.
        #[arg(long, value_name = "LUMA", value_parser = clap::value_parser!(u16).range(5..=50))]
        stealth_intensity: Option<u16>,
        /// Double the stealth data rate: sixteen modulation levels (four bits
        /// per block pair) over the same luma span instead of four levels.
        /// The level spacing narrows, so pair this with a higher
        /// --stealth-intensity (40+) for aggressive distribution channels.
        #[arg(long)]
        stealth_dense: bool,
        /// Leave the looping background's audio out of the output instead of
        /// muxing it as AAC (default: keep it in sync with the video).
        #[arg(long)]
        stealth_no_audio: bool,
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
    /// Benchmark encode/decode configurations on this machine (backend x mode).
    Benchmark {
        /// Use this input file instead of generated random payload.
        input: Option<PathBuf>,
        /// MiB of random payload to generate when --input is not used.
        #[arg(long, default_value_t = 8)]
        size: u64,
        #[command(flatten)]
        settings: Encoding,
    },
}

fn password_for(program: &str) -> Password {
    Password {
        password: Some(program.to_owned()),
        password_file: None,
    }
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
    // Borrows the stored password so a failed regular decode can retry the
    // same credentials in stealth mode.
    fn get(&self, confirm: bool) -> Result<Zeroizing<Vec<u8>>> {
        let value = Zeroizing::new(if let Some(s) = &self.password {
            s.clone()
        } else if let Some(path) = &self.password_file {
            const LIMIT: u64 = 65536;
            let mut text = String::new();
            File::open(path)?
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

#[derive(Clone, Args)]
struct Encoding {
    #[arg(long, value_name = "PIXELS")]
    width: Option<usize>,
    #[arg(long, value_name = "PIXELS")]
    height: Option<usize>,
    #[arg(long, value_name = "PIXELS")]
    block_size: Option<usize>,
    #[arg(long, value_parser = clap::value_parser!(u8).range(1..=60))]
    fps: Option<u8>,
    /// Use a smaller-output profile (can reduce recovery margin).
    #[arg(long, value_enum)]
    compress: Option<Profile>,
    /// x264 quality (default: 8; capped at 8 so every cell survives quantization).
    #[arg(long, value_parser = clap::value_parser!(u8).range(0..=51))]
    crf: Option<u8>,
    /// x264 speed/size preset (default: faster, or set by --compress, for the CPU fallback and GPU speed tiers).
    #[arg(long, value_parser = clap::builder::PossibleValuesParser::new(config::PRESETS))]
    preset: Option<String>,
    /// Reed-Solomon parity bytes per 255-byte codeword (default: 32, or set by --compress).
    #[arg(long, value_parser = clap::value_parser!(u8).range(1..=127))]
    ecc_symbols: Option<u8>,
    /// Store payload cells in the U and V chroma planes too (+50% capacity per
    /// frame, ~33% fewer frames, same MP4 size; verify with a round trip on
    /// your GPU encoder first).
    #[arg(long, num_args(0..=1), default_missing_value = "true")]
    chroma: Option<bool>,
    /// Encode every frame as a keyframe (all-intra): smaller at CRF 6 and
    /// faster to decode, 10x more cells for error correction to watch.
    #[arg(long, num_args(0..=1), default_missing_value = "true")]
    intra: Option<bool>,
    /// Automatically pick the best measured backend and frame mode for this
    /// machine (default: off, or set by the config file).
    #[arg(long, value_enum, value_name = "TARGET")]
    optimise: Option<Target>,
    /// Number of PBKDF2-HMAC-SHA256 iterations.
    #[arg(long, value_parser = clap::value_parser!(u32).range(1..=i64::from(MAX_KDF)))]
    pbkdf2_iterations: Option<u32>,
}

impl Encoding {
    /// What to optimise for: an explicit flag wins over the config file.
    fn optimise_target(&self, config: &Config) -> Target {
        self.optimise.or(config.optimise).unwrap_or(Target::Off)
    }

    fn resolve(&self, config: &Config) -> Result<(video::Settings, usize, u32)> {
        // Compact mode draws the transport header into the cell video, which
        // is at least 256x256 pixels, so only the display grid's own layout
        // needs to be valid here; demanding room for the header on the
        // display grid wrongly rejected small resolutions (e.g. 320x240).
        let display = Grid {
            width: self.width.or(config.width).unwrap_or(1920),
            height: self.height.or(config.height).unwrap_or(1080),
            block: self.block_size.or(config.block_size).unwrap_or(8),
        }
        .validate_layout()?;
        // Compact mode stores one cell per video pixel at cell resolution;
        // the padded cells carry stream data just like any other cell.
        let (crf, preset, parity) = match self.compress.or(config.compress) {
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
        ensure!(
            grid.bits() >= TRANSPORT_SIZE * 8,
            "the cell video is too small for the video header"
        );
        Ok((
            video::Settings {
                grid,
                display,
                fps: self.fps.or(config.fps).unwrap_or(30),
                crf: self.crf.or(config.crf).unwrap_or(crf),
                preset: self
                    .preset
                    .clone()
                    .or_else(|| config.preset.clone())
                    .unwrap_or_else(|| preset.to_owned()),
                cell_bits: 4,
                compact: true,
                chroma: self.chroma.or(config.chroma).unwrap_or(false),
                intra: self.intra.or(config.intra).unwrap_or(false),
            },
            self.ecc_symbols
                .or(config.ecc_symbols)
                .map_or(parity, usize::from),
            self.pbkdf2_iterations
                .or(config.pbkdf2_iterations)
                .unwrap_or(600_000),
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

fn mode_name(intra: bool, chroma: bool) -> &'static str {
    MODES
        .iter()
        .find(|(_, i, c)| *i == intra && *c == chroma)
        .map(|(label, _, _)| *label)
        .unwrap_or("custom")
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

// Stages the plaintext (metadata + file content) into a scratch file: either
// encrypted under the given password, or in the clear with a SHA-256
// integrity hash when no password is used. Returns the 128-byte video header
// plus the staged stream; the header bytes are what the video writers
// prepend to the payload stream.
fn stage_stream<R: Read>(
    source: R,
    plain_size: u64,
    encrypt: Option<(&Zeroizing<Vec<u8>>, u32)>,
    dir: &Path,
) -> Result<(crypto::Header, File)> {
    match encrypt {
        Some((password, iterations)) => {
            encrypt_stream(source, plain_size, password, iterations, dir)
        }
        None => stage_plain(source, plain_size, dir),
    }
}

fn encrypt_stream<R: Read>(
    mut source: R,
    plain_size: u64,
    password: &Zeroizing<Vec<u8>>,
    iterations: u32,
    dir: &Path,
) -> Result<(crypto::Header, File)> {
    let mut header = crypto::Header::new(iterations, plain_size)?;
    let mut crypto = crypto::Stream::new(&header, password);
    let mut staged = tempfile::tempfile_in(dir).with_context(|| {
        format!(
            "could not create ciphertext scratch file in {}",
            dir.display()
        )
    })?;
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
    Ok((header, staged))
}

fn stage_plain<R: Read>(
    mut source: R,
    plain_size: u64,
    dir: &Path,
) -> Result<(crypto::Header, File)> {
    let mut header = crypto::Header::plain(plain_size)?;
    let mut plain = crypto::Plain::new(plain_size);
    let mut staged = tempfile::tempfile_in(dir)
        .with_context(|| format!("could not create scratch file in {}", dir.display()))?;
    let mut buffer = Zeroizing::new(vec![0; CHUNK]);
    let mut left = plain_size;
    while left != 0 {
        let n = left.min(CHUNK as u64) as usize;
        let chunk = &mut buffer[..n];
        source
            .read_exact(chunk)
            .context("could not read input file (it may have shrunk)")?;
        plain.update(chunk)?;
        staged
            .write_all(chunk)
            .context("could not stage the file (check free disk space)")?;
        left -= n as u64;
    }
    ensure!(
        source.read(&mut [0])? == 0,
        "input file grew while encoding"
    );
    header.hash = plain.finish();
    Ok((header, staged))
}

fn encode(
    tools: &video::Tools,
    input: &Path,
    output: &Path,
    password: Password,
    no_password: bool,
    options: Encoding,
    ctx: &Ctx,
) -> Result<u64> {
    let (settings, parity, iterations) = options.resolve(ctx.config)?;
    check_output(output)?;
    regular_file_len(fs::metadata(input)?)?;
    let file = File::open(input)?;
    let size = regular_file_len(file.metadata()?)?;
    let metadata = format::metadata(input, size)?;
    let plain_size = size
        .checked_add(metadata.len() as u64)
        .context("input size overflow")?;
    let encrypted_size = plain_size + CRYPTO_SIZE as u64;
    let layout = if settings.compact {
        Layout::compact(encrypted_size, parity)?
    } else {
        Layout::new(encrypted_size, parity)?
    };
    let password = if no_password {
        None
    } else {
        Some(password.get(true)?)
    };

    let mut tools = tools.clone();
    let original_gpu = tools.gpu;
    let fallback = settings.clone();
    let settings = apply_optimisation(&mut tools, &settings, &options, ctx);
    let replaced = settings.chroma != fallback.chroma
        || settings.intra != fallback.intra
        || tools.gpu != original_gpu;
    let frames = layout.frames(settings.grid, settings.cell_bits, settings.chroma);
    if !ctx.quiet {
        eprintln!(
            "{}; writing {frames} video frames ({} luma levels per cell{}).",
            if no_password {
                format!("Storing {size} bytes without a password (integrity hash only)")
            } else {
                format!("Encrypting {size} bytes")
            },
            1 << settings.cell_bits,
            if settings.chroma {
                ", data in the chroma planes too"
            } else {
                ""
            }
        );
    }

    // The format places the final GCM tag before the ciphertext. Stage only
    // the ciphertext (never the input file) so the original is read once.
    let dir = parent(output);
    let (header, mut staged) = stage_stream(
        metadata.as_slice().chain(file),
        plain_size,
        password.as_ref().map(|password| (password, iterations)),
        dir,
    )?;
    drop(password);
    let ecc_header = layout.ecc_header(parity);
    let mut written = None;
    let mut attempt = settings;
    for round in 0..2 {
        staged.rewind()?;
        let bytes = header.bytes();
        let attempt_frames = layout.frames(attempt.grid, attempt.cell_bits, attempt.chroma);
        let mut stream = bytes.as_slice().chain(&mut staged);
        match write_video(
            &tools,
            &attempt,
            dir,
            parity,
            layout.protected,
            attempt_frames,
            &ecc_header,
            layout.encrypted,
            &mut stream,
        ) {
            Ok(tmp) => {
                written = Some(tmp);
                break;
            }
            Err(error) => {
                if round == 0 && replaced {
                    eprintln!(
                        "Warning: the optimised video mode failed ({error:#}); retrying with {} and {}.",
                        original_gpu.short_name(),
                        mode_name(fallback.intra, fallback.chroma),
                    );
                    attempt = fallback.clone();
                    tools.gpu = original_gpu;
                } else {
                    return Err(error);
                }
            }
        }
    }
    publish(written.expect("a video write attempt succeeded"), output)?;
    Ok(frames)
}

#[allow(clippy::too_many_arguments)]
fn write_video<R: Read>(
    tools: &video::Tools,
    settings: &video::Settings,
    dir: &Path,
    parity: usize,
    protected: u64,
    frames: u64,
    ecc_header: &[u8],
    stream_len: u64,
    stream: R,
) -> Result<NamedTempFile> {
    let transport = format::transport(
        settings.display,
        parity,
        protected,
        settings.cell_bits,
        settings.compact,
        settings.chroma,
    );
    let tmp = temporary(dir)?;
    (|| -> Result<NamedTempFile> {
        let mut process = tools.encode(settings, tmp.path())?;
        let pipe = process.child.stdin.take().context("missing FFmpeg stdin")?;
        let pushed = (|| -> Result<()> {
            let mut sink = video::Sink::new(pipe, settings.grid, settings.chroma, frames);
            sink.push(&transport)?;
            let codec = ecc::Codec::new(parity);
            if settings.compact {
                // In compact mode the ECC header leads the first
                // Reed-Solomon word's data, so parity protects it against
                // quantization noise too.
                codec.encode(
                    ecc_header.chain(stream),
                    ECC_SIZE as u64 + stream_len,
                    |data| sink.push(data),
                )?;
            } else {
                sink.push(ecc_header)?;
                codec.encode(stream, stream_len, |data| sink.push(data))?;
            }
            sink.finish()?;
            Ok(())
        })();
        if let Err(error) = pushed {
            return Err(process.explain(error));
        }
        process.finish()?;
        Ok(tmp)
    })()
}

/// Encodes the file as a faint differential-luma data layer over a looping
/// background video. Calibration profiles do not apply: the stealth layer is
/// modulated into normal gameplay footage, not into cells.
#[allow(clippy::too_many_arguments)]
fn encode_stealth(
    tools: &video::Tools,
    input: &Path,
    output: &Path,
    background: &Path,
    password: Password,
    no_password: bool,
    stealth_block: Option<u16>,
    stealth_intensity: Option<u16>,
    stealth_dense: bool,
    stealth_no_audio: bool,
    options: Encoding,
    ctx: &Ctx,
) -> Result<u64> {
    // The looping background's audio is muxed into the output by default so
    // the stealth video looks (and sounds) like the original footage; an
    // explicit flag or config default drops it and saves the AAC bitrate.
    let keep_audio = !(stealth_no_audio || ctx.config.stealth_no_audio == Some(true));
    let block = usize::from(
        stealth_block
            .or_else(|| ctx.config.stealth_block.map(|block| block as u16))
            .unwrap_or(u16::try_from(stealth::BLOCK).unwrap()),
    );
    ensure!(
        block.is_multiple_of(2),
        "--stealth-block must be an even number of pixels"
    );
    let intensity = i16::try_from(
        stealth_intensity
            .map(usize::from)
            .or(ctx.config.stealth_intensity)
            .unwrap_or(stealth::INTENSITY as usize),
    )
    .context("--stealth-intensity out of range")?;
    let symbol_bits = if stealth_dense || ctx.config.stealth_dense == Some(true) {
        4
    } else {
        2
    };
    // The chroma planes carry the background colours in stealth encoding, so
    // any chroma request (explicit flag or config default) is ignored with a
    // warning; a config default must never block a stealth encode. An
    // optimise target is applied instead of refused: the calibrated backend
    // and all-intra choice transfer, only its chroma part never does.
    let chroma_requested = options.chroma.is_some_and(|chroma| chroma)
        || (options.chroma.is_none() && ctx.config.chroma == Some(true));
    if chroma_requested && !ctx.quiet {
        eprintln!(
            "Warning: ignoring the chroma setting; the chroma planes carry the background colours in stealth encoding."
        );
    }
    let mut tools = tools.clone();
    let intra = optimise_for_stealth(
        &mut tools,
        &options,
        ctx,
        options.intra.or(ctx.config.intra).unwrap_or(false),
    );
    let (bg_width, bg_height) = tools
        .dimensions(background)
        .context("could not probe the stealth background video")?;
    let width = options
        .width
        .or(ctx.config.width)
        .unwrap_or_else(|| bg_width / block * block);
    let height = options
        .height
        .or(ctx.config.height)
        .unwrap_or_else(|| bg_height / block * block);
    let geometry = stealth::Geometry::build(width, height, block, symbol_bits, intensity)
        .context("the stealth geometry does not fit these settings")?;
    let fps = options.fps.or(ctx.config.fps).unwrap_or(30);
    let crf = options.crf.or(ctx.config.crf).unwrap_or(18);
    let preset = options
        .preset
        .clone()
        .or_else(|| ctx.config.preset.clone())
        .unwrap_or_else(|| "faster".to_owned());
    // Stealth defaults to 64 Reed-Solomon parity symbols: the extra symbols
    // of the older 96 default never earned their keep in measured round
    // trips (a re-encoded dense layer fails both settings), while they cost
    // ~25% more frames and output size.
    let parity = usize::from(options.ecc_symbols.or(ctx.config.ecc_symbols).unwrap_or(64));
    let iterations = options
        .pbkdf2_iterations
        .or(ctx.config.pbkdf2_iterations)
        .unwrap_or(600_000);
    check_output(output)?;
    regular_file_len(fs::metadata(input)?)?;
    let file = File::open(input)?;
    let size = regular_file_len(file.metadata()?)?;
    let metadata = format::metadata(input, size)?;
    let plain_size = size
        .checked_add(metadata.len() as u64)
        .context("input size overflow")?;
    let encrypted_size = plain_size + CRYPTO_SIZE as u64;
    let layout = Layout::compact(encrypted_size, parity)?;
    let password = if no_password {
        None
    } else {
        Some(password.get(true)?)
    };
    let dir = parent(output);
    let (header, mut staged) = stage_stream(
        metadata.as_slice().chain(file),
        plain_size,
        password.as_ref().map(|password| (password, iterations)),
        dir,
    )?;
    drop(password);
    staged.rewind()?;
    let frames = (8 + TRANSPORT_SIZE as u64 + layout.protected - ECC_SIZE as u64)
        .div_ceil(u64::try_from(geometry.frame_bytes()).context("frame capacity overflow")?);
    if !ctx.quiet {
        eprintln!(
            "Encoding {size} bytes as a stealth layer over {} at {}x{}, block {block}{} at intensity {intensity}, {} fps, CRF {crf}, {parity} parity symbols{}; writing {frames} frames.",
            background.display(),
            width,
            height,
            if symbol_bits == 4 { ", dense" } else { "" },
            fps,
            if keep_audio {
                ", keeping the background audio"
            } else {
                ", without background audio"
            }
        );
        // The data layer hides near-random noise, so the MP4 runs at a
        // multiple of the payload; the level spacing decides how much
        // compression noise it survives. Measured restore boundaries: a
        // spacing of ~3.3 luma levels (dense, intensity 25) restores up to
        // about CRF 17 and ~6.7 levels up to about CRF 20; each doubling of
        // the spacing buys roughly 3.5 CRF more.
        // Blocks under 4 px carry four times the data per frame, but the
        // H.264 deblocking filter smooths the 2 px pattern away: measured
        // round trips at 720p failed even at CRF 12 with all-intra, and only
        // a near-lossless CRF 6 kept the layer readable. Warn before someone
        // encodes a full archive into a video nobody can restore.
        if block < 4 {
            eprintln!(
                "Warning: --stealth-block {block} rarely survives a real encode; the H.264 deblocking filter erases the 2 px pattern unless the encode is near lossless (--crf 6 --intra --stealth-intensity 50). Prefer the default block 4."
            );
        }
        let step_luma = 2.0 * f64::from(intensity) / ((1usize << symbol_bits) - 1) as f64;
        let safe_crf = (17.0 + 3.5 * (step_luma / 3.33).log2()) as u8;
        if crf > safe_crf {
            eprintln!(
                "Warning: with {symbol_bits} bits per pair at intensity {intensity} the level spacing is {step_luma:.1} luma levels; CRF {crf} is usually too lossy to restore reliably. Use --crf {safe_crf} or lower, a higher --stealth-intensity, or drop --stealth-dense."
            );
        }
        if parity > 64 {
            eprintln!(
                "Note: the stealth MP4 goes several times larger than the payload; {parity} parity symbols add ~1% stream size per symbol over the 64 default -- lower --stealth-intensity too if size matters."
            );
        }
    }
    let crypto_header = header.bytes();
    let mut stream = crypto_header.as_slice().chain(&mut staged);
    let tmp = stealth::encode(
        &tools,
        background,
        &stealth::StealthSettings {
            width,
            height,
            rescale: width != bg_width || height != bg_height,
            block,
            symbol_bits,
            intensity,
            fps,
            crf,
            preset,
            intra,
            audio: keep_audio,
            parity,
        },
        layout,
        dir,
        &mut stream,
    )?;
    publish(tmp, output)?;
    Ok(frames)
}

fn quick_calibration(tools: &video::Tools, options: &Encoding, ctx: &Ctx) -> Result<Calibration> {
    let temp = tempfile::tempdir().context("could not create a calibration directory")?;
    let payload = temp.path().join("calibration.bin");
    generate_payload(&payload, CALIBRATION_PAYLOAD_MIB)?;
    eprintln!(
        "Calibrating the encoders with {:.0} MiB of generated payload...",
        CALIBRATION_PAYLOAD_MIB
    );
    let bytes = regular_file_len(fs::metadata(&payload)?)?;
    let rows = run_matrix(tools, &payload, bytes, options, ctx);
    let calibration = calibration_from(diag::fingerprint(&tools.ffmpeg), &rows);
    if let Err(error) = calibration.save(ctx.calibration) {
        eprintln!(
            "Warning: the calibration could not be saved ({error:#}); the results are used for this run only."
        );
    }
    Ok(calibration)
}

/// Rewrites the settings with the best measured backend and frame mode for
/// this machine, calibrating on the fly when the saved results are missing
/// or refer to different hardware. `Off` keeps the configured settings.
fn apply_optimisation(
    tools: &mut video::Tools,
    settings: &video::Settings,
    options: &Encoding,
    ctx: &Ctx,
) -> video::Settings {
    let target = options.optimise_target(ctx.config);
    if target == Target::Off {
        return settings.clone();
    }
    if !ctx.quiet {
        eprintln!("Optimising video encoding for {}...", target.label());
    }
    let fingerprint = diag::fingerprint(&tools.ffmpeg);
    let calibration = match Calibration::load(ctx.calibration) {
        Ok(Some(saved)) if saved.is_fresh(&fingerprint) => saved,
        _ => match quick_calibration(tools, options, ctx) {
            Ok(fresh) => fresh,
            Err(error) => {
                eprintln!(
                    "Warning: calibration failed ({error:#}); using the configured settings."
                );
                return settings.clone();
            }
        },
    };
    let chosen = match calibration.choose(
        target,
        forced_backend(tools.gpu),
        options.intra,
        options.chroma,
    ) {
        Some(measurement) => measurement,
        None => {
            if !ctx.quiet {
                eprintln!(
                    "No measured candidate matches the configured settings; keeping {} {}.",
                    tools.gpu.short_name(),
                    mode_name(settings.intra, settings.chroma)
                );
            }
            return settings.clone();
        }
    };
    if !ctx.quiet {
        eprintln!(
            "Using {}{} for this machine ({:.2}s per {:.0} MiB, {:.0}% file size in the benchmark).",
            chosen.backend,
            match mode_name(chosen.intra, chosen.chroma) {
                "base" => String::new(),
                label => format!(" {label}"),
            },
            chosen.encode_s + chosen.decode_s,
            chosen.payload as f64 / (1024.0 * 1024.0),
            (chosen.mp4 as f64 / chosen.payload.max(1) as f64) * 100.0
        );
    }
    if let Some(backend) = video::Gpu::from_name(&chosen.backend) {
        tools.gpu = backend;
    }
    video::Settings {
        chroma: chosen.chroma,
        intra: chosen.intra,
        ..settings.clone()
    }
}

/// Applies the optimisation target to a stealth encode instead of refusing
/// it: the calibrated backend and all-intra choice transfer to the stealth
/// layer. Only the chroma frame mode never applies there, because the
/// chroma planes carry the background colours. The profiles are measured on
/// the cell format, so the choice is a hardware-level hint rather than an
/// exact stealth measurement; the warning says as much. Returns the
/// all-intra choice for the stealth encode.
fn optimise_for_stealth(
    tools: &mut video::Tools,
    options: &Encoding,
    ctx: &Ctx,
    fallback_intra: bool,
) -> bool {
    let target = options.optimise_target(ctx.config);
    if target == Target::Off {
        return fallback_intra;
    }
    if !ctx.quiet {
        eprintln!(
            "Warning: the optimise profiles are calibrated on the cell format; for stealth encoding only the measured backend and all-intra choice apply (the chroma mode never does)."
        );
        eprintln!("Optimising video encoding for {}...", target.label());
    }
    let fingerprint = diag::fingerprint(&tools.ffmpeg);
    let calibration = match Calibration::load(ctx.calibration) {
        Ok(Some(saved)) if saved.is_fresh(&fingerprint) => saved,
        _ => match quick_calibration(tools, options, ctx) {
            Ok(fresh) => fresh,
            Err(error) => {
                eprintln!(
                    "Warning: calibration failed ({error:#}); using the configured settings."
                );
                return fallback_intra;
            }
        },
    };
    let chosen = match calibration.choose(
        target,
        forced_backend(tools.gpu),
        options.intra,
        // Candidates that store data in the chroma planes cannot apply.
        Some(false),
    ) {
        Some(chosen) => chosen,
        None => {
            if !ctx.quiet {
                eprintln!(
                    "No measured candidate matches the configured settings; keeping {} {}.",
                    tools.gpu.short_name(),
                    mode_name(fallback_intra, false)
                );
            }
            return fallback_intra;
        }
    };
    if !ctx.quiet {
        eprintln!(
            "Using {}{} for this machine ({:.2}s per {:.0} MiB, {:.0}% file size in the benchmark).",
            chosen.backend,
            match mode_name(chosen.intra, chosen.chroma) {
                "base" => String::new(),
                label => format!(" {label}"),
            },
            chosen.encode_s + chosen.decode_s,
            chosen.payload as f64 / (1024.0 * 1024.0),
            (chosen.mp4 as f64 / chosen.payload.max(1) as f64) * 100.0
        );
    }
    if let Some(backend) = video::Gpu::from_name(&chosen.backend) {
        tools.gpu = backend;
    }
    chosen.intra
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

const NOT_A_FILE2VIDEO_VIDEO: &str = "not a file2video video; wrong layout or damaged first frame";

/// Decodes a regular (cell-based) video, and falls back to the blind stealth
/// decoder automatically when the first frame carries no cell layout.
fn decode(
    tools: &video::Tools,
    input: &Path,
    output: &Path,
    password: &Password,
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
    match decode_regular(
        tools, input, output, password, &options, width, height, dir, is_dir,
    ) {
        Err(error) if error.to_string().contains(NOT_A_FILE2VIDEO_VIDEO) => {
            eprintln!("Not a regular file2video video; trying the stealth decoder...");
            decode_stealth(
                tools, input, output, password, &options, width, height, dir, is_dir,
            )
        }
        result => result,
    }
}

#[allow(clippy::too_many_arguments)]
fn decode_regular(
    tools: &video::Tools,
    input: &Path,
    output: &Path,
    password: &Password,
    options: &DecodeOptions,
    width: usize,
    height: usize,
    dir: &Path,
    is_dir: bool,
) -> Result<PathBuf> {
    let mut process = tools.decode(input)?;
    let pipe = process
        .child
        .stdout
        .take()
        .context("missing FFmpeg stdout")?;
    let (mut frames, parity, protected) = video::Source::open(pipe, width, height, options.block)?;
    let chroma = frames.chroma();
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
        .set_frames(layout.frames(grid, cell_bits, chroma));
    let (tmp, name) = decrypt_staged(reader, layout, password, dir)?;
    frames.finish()?;
    process.finish()?;
    let destination = destination_for(name, output, dir, is_dir, options.restore_name)?;
    publish(tmp, &destination)?;
    Ok(destination)
}

#[allow(clippy::too_many_arguments)]
fn decode_stealth(
    tools: &video::Tools,
    input: &Path,
    output: &Path,
    password: &Password,
    options: &DecodeOptions,
    width: usize,
    height: usize,
    dir: &Path,
    is_dir: bool,
) -> Result<PathBuf> {
    let mut process = tools.decode(input)?;
    let pipe = process
        .child
        .stdout
        .take()
        .context("missing FFmpeg stdout")?;
    let mut reader = stealth::Reader::open(pipe, width, height)?;
    let parity = reader.parity();
    let protected = reader.protected();
    let codec = ecc::Codec::new(parity);
    // Plain allowance for the codewords: words carry 255 - parity data
    // bytes each, which is all the encoder ever wrote.
    let words = (protected - ECC_SIZE as u64) / 255;
    let mut stream = codec.reader(&mut reader, words * (255 - parity) as u64);
    let mut ecc_header = [0; ECC_SIZE];
    stream.read_exact(&mut ecc_header)?;
    let layout = Layout::parse(&ecc_header, parity, protected, true)?;
    let (tmp, name) = decrypt_staged(stream, layout, password, dir)?;
    reader.finish()?;
    process.finish()?;
    let destination = destination_for(name, output, dir, is_dir, options.restore_name)?;
    publish(tmp, &destination)?;
    Ok(destination)
}

/// Validates and stages the plaintext from an ECC reader; the reader must
/// already sit at the first codeword byte. The 128-byte video header says
/// whether the bytes that follow are still to be decrypted.
fn decrypt_staged<R: Read>(
    mut reader: ecc::Reader<'_, R>,
    layout: Layout,
    password: &Password,
    dir: &Path,
) -> Result<(NamedTempFile, Option<String>)> {
    let mut header = [0; CRYPTO_SIZE];
    reader.read_exact(&mut header)?;
    let header = crypto::Header::parse(&header, layout.encrypted - CRYPTO_SIZE as u64)?;
    // Ask for a password only when the video actually carries ciphertext.
    let mut unseal = if header.sealed {
        let password = password.get(false)?;
        Unseal::Sealed(Box::new(crypto::Stream::new(&header, &password)))
    } else {
        Unseal::Plain(crypto::Plain::new(header.size))
    };
    let mut tmp = temporary(dir)?;
    let mut buffer = Zeroizing::new(vec![0; CHUNK]);

    let n = header.size.min(CHUNK as u64) as usize;
    reader.read_exact(&mut buffer[..n])?;
    unseal.update(&mut buffer[..n])?;
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
        unseal.update(chunk)?;
        tmp.write_all(chunk)?;
        left -= n as u64;
    }
    unseal.verify(&header)?;
    let (name, _) = metadata?;
    drop(reader);
    Ok((tmp, name))
}

enum Unseal {
    Sealed(Box<crypto::Stream>),
    Plain(crypto::Plain),
}

impl Unseal {
    fn update(&mut self, data: &mut [u8]) -> Result<()> {
        match self {
            Unseal::Sealed(crypto) => crypto.update(data, false),
            Unseal::Plain(plain) => plain.update(data),
        }
    }

    fn verify(self, header: &crypto::Header) -> Result<()> {
        match self {
            Unseal::Sealed(crypto) => crypto.verify(header),
            Unseal::Plain(plain) => plain.verify(header),
        }
    }
}

fn destination_for(
    name: Option<String>,
    output: &Path,
    dir: &Path,
    is_dir: bool,
    restore_name: bool,
) -> Result<PathBuf> {
    match (name, is_dir, restore_name) {
        (Some(name), true, _) | (Some(name), _, true) => Ok(dir.join(restorable_name(&name)?)),
        (None, true, _) => bail!("video has no stored filename; specify an output filename"),
        _ => Ok(output.to_owned()),
    }
}

fn estimate(tools: &video::Tools, input: &Path, options: Encoding, ctx: &Ctx) -> Result<()> {
    let target = options.optimise_target(ctx.config);
    let mut optimise_note: Option<String> = None;
    if target != Target::Off {
        let fingerprint = diag::fingerprint(&tools.ffmpeg);
        optimise_note = Some(match Calibration::load(ctx.calibration) {
            Ok(Some(saved)) if saved.is_fresh(&fingerprint) => {
                let forced = forced_backend(tools.gpu);
                match saved.choose(target, forced, options.intra, options.chroma) {
                    Some(measurement) => format!(
                        "Optimising for {} would use: {} {}.",
                        target.label(),
                        measurement.backend,
                        mode_name(measurement.intra, measurement.chroma)
                    ),
                    None => "No measured candidate matches these settings; run `file2video benchmark` with the same flags first.".to_owned(),
                }
            }
            Ok(None) => "The calibration file does not exist yet; run `file2video benchmark` to measure this machine.".to_owned(),
            Ok(Some(_)) => "The calibration file refers to other hardware; run `file2video benchmark` to measure this machine again.".to_owned(),
            Err(error) => format!(
                "The calibration file could not be read ({error:#}); run `file2video benchmark` to recreate it."
            ),
        });
    }
    let (settings, parity, _) = options.resolve(ctx.config)?;
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
    let frames = layout.frames(settings.grid, settings.cell_bits, settings.chroma);
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
    if let Some(note) = optimise_note {
        println!("{note}");
    }
    Ok(())
}

fn forced_backend(gpu: video::Gpu) -> Option<&'static str> {
    if gpu == video::Gpu::Auto {
        None
    } else {
        Some(gpu.short_name())
    }
}

fn generate_payload(path: &Path, mib: u64) -> Result<u64> {
    ensure!(mib > 0, "--size must be positive");
    let bytes = mib.saturating_mul(1024 * 1024);
    let mut state = 0x243f_6a88_85a3_08d3u64;
    let mut file =
        File::create(path).with_context(|| format!("could not create {}", path.display()))?;
    let mut buffer = vec![0u8; CHUNK];
    let mut left = bytes;
    while left != 0 {
        let n = (left as usize).min(buffer.len());
        let chunk = &mut buffer[..n];
        for word in chunk.as_chunks_mut::<8>().0 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            word.copy_from_slice(&state.to_le_bytes());
        }
        for byte in chunk.as_chunks_mut::<8>().1 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            *byte = state as u8;
        }
        file.write_all(chunk)
            .with_context(|| format!("could not write {}", path.display()))?;
        left -= n as u64;
    }
    Ok(bytes)
}

fn files_equal(a: &Path, b: &Path) -> Result<bool> {
    let mut left = BufReader::new(File::open(a)?);
    let mut right = BufReader::new(File::open(b)?);
    let (mut one, mut two) = ([0; 64 * 1024], [0; 64 * 1024]);
    loop {
        let l = left.read(&mut one)?;
        let r = right.read(&mut two)?;
        if l != r || one[..l] != two[..r] {
            return Ok(false);
        }
        if l == 0 {
            return Ok(true);
        }
    }
}

enum Row {
    Measured(calibrate::Measurement),
    Failed {
        failure: calibrate::Failure,
        error: String,
    },
}

/// Every usable backend for the current `--gpu` choice, ending with the
/// CPU fallback.
fn backend_list(tools: &video::Tools) -> Vec<(&'static str, video::Gpu)> {
    let mut backends: Vec<(&'static str, video::Gpu)> = Vec::new();
    if tools.gpu != video::Gpu::Off {
        if tools.gpu == video::Gpu::Auto {
            for backend in tools.available_backends() {
                backends.push((backend.short_name(), backend));
            }
        } else {
            backends.push((tools.gpu.short_name(), tools.gpu));
        }
    }
    backends.push(("cpu", video::Gpu::Off));
    backends
}

/// Encodes and decodes the payload with every backend x frame-mode
/// combination, returning one row per combination. This is the shared engine
/// for `benchmark` and for the automatic calibration inside `encode`.
fn run_matrix(
    tools: &video::Tools,
    payload: &Path,
    bytes: u64,
    base: &Encoding,
    ctx: &Ctx,
) -> Vec<Row> {
    let quiet = Ctx {
        config: ctx.config,
        calibration: ctx.calibration,
        quiet: true,
    };
    let temp = match tempfile::tempdir() {
        Ok(temp) => temp,
        Err(_) => return Vec::new(),
    };
    let mut rows = Vec::new();
    for (name, gpu) in backend_list(tools) {
        let run_tools = video::Tools {
            ffmpeg: tools.ffmpeg.clone(),
            ffprobe: tools.ffprobe.clone(),
            threads: tools.threads,
            gpu,
        };
        for (_, intra, chroma) in MODES {
            let mut bench = base.clone();
            bench.optimise = Some(Target::Off);
            bench.intra = Some(intra);
            bench.chroma = Some(chroma);
            bench.crf = None;
            bench.pbkdf2_iterations = Some(1000);
            let run = rows.len() + 1;
            let video_path = temp.path().join(format!("bench-{run}.mp4"));
            let restored = temp.path().join(format!("restored-{run}"));
            let started = Instant::now();
            let outcome = encode(
                &run_tools,
                payload,
                &video_path,
                password_for("file2video benchmark"),
                false,
                bench,
                &quiet,
            )
            .map(|frames| {
                let encode_time = started.elapsed().as_secs_f64();
                let mp4 = fs::metadata(&video_path).map(|m| m.len()).unwrap_or(0);
                let decode_started = Instant::now();
                decode(
                    &run_tools,
                    &video_path,
                    &restored,
                    &password_for("file2video benchmark"),
                    DecodeOptions {
                        restore_name: false,
                        width: None,
                        height: None,
                        block: None,
                    },
                )
                .map(|_| {
                    let decode_time = decode_started.elapsed().as_secs_f64();
                    (frames, encode_time, decode_time, mp4)
                })
            });
            match outcome {
                Ok(Ok((frames, encode_time, decode_time, mp4))) => {
                    if files_equal(payload, &restored).unwrap_or(false) {
                        rows.push(Row::Measured(calibrate::Measurement {
                            backend: name.to_owned(),
                            intra,
                            chroma,
                            frames,
                            payload: bytes,
                            encode_s: encode_time,
                            decode_s: decode_time,
                            mp4,
                        }));
                    } else {
                        rows.push(Row::Failed {
                            failure: calibrate::Failure {
                                backend: name.to_owned(),
                                intra,
                                chroma,
                            },
                            error: "decoded output does not match the payload".to_owned(),
                        });
                    }
                }
                Ok(Err(error)) | Err(error) => rows.push(Row::Failed {
                    failure: calibrate::Failure {
                        backend: name.to_owned(),
                        intra,
                        chroma,
                    },
                    error: format!("{error:#}"),
                }),
            }
        }
    }
    rows
}

fn as_table(rows: &[Row], bytes: u64) {
    for row in rows {
        match row {
            Row::Measured(measurement) => println!(
                "{:<22} {:<15} {:>7} {:>8.2}s {:>10} {:>8.2}s {:>10} {:>8} MB {:>6}%",
                measurement.backend,
                mode_name(measurement.intra, measurement.chroma),
                measurement.frames,
                measurement.encode_s,
                speed(bytes, measurement.encode_s),
                measurement.decode_s,
                speed(bytes, measurement.decode_s),
                format!("{:.1}", measurement.mp4 as f64 / (1024.0 * 1024.0)),
                format!("{:.0}", (measurement.mp4 as f64 / bytes as f64) * 100.0)
            ),
            Row::Failed { failure, error } => {
                println!(
                    "{:<22} {:<15} {:>7} {:>8} {:>10} {:>8} {:>10} {:>8} MB {:>6}%",
                    failure.backend,
                    mode_name(failure.intra, failure.chroma),
                    "-",
                    "-",
                    "-",
                    "-",
                    "-",
                    "-",
                    "-"
                );
                eprintln!(
                    "{} {}: {error}",
                    failure.backend,
                    mode_name(failure.intra, failure.chroma)
                );
            }
        }
    }
}

fn calibration_from(fingerprint: String, rows: &[Row]) -> Calibration {
    let mut calibration = Calibration::new(fingerprint);
    for row in rows {
        match row {
            Row::Measured(measurement) => calibration.measurements.push(measurement.clone()),
            Row::Failed { failure, .. } => calibration.failures.push(failure.clone()),
        }
    }
    calibration
}

fn benchmark(
    tools: &video::Tools,
    input: Option<PathBuf>,
    size: u64,
    options: Encoding,
    ctx: &Ctx,
) -> Result<()> {
    let temp = tempfile::tempdir().context("could not create a benchmark directory")?;
    let payload: PathBuf = match input {
        Some(path) => path,
        None => {
            let path = temp.path().join("payload.bin");
            eprintln!("Generating {size} MiB of pseudo-random payload...");
            generate_payload(&path, size)?;
            path
        }
    };
    let bytes = regular_file_len(fs::metadata(&payload)?)?;
    println!(
        "Payload: {:.1} MB | {} fps | {} parity symbols | CRF cap 8 (intra: 6)",
        bytes as f64 / (1024.0 * 1024.0),
        options.fps.or(ctx.config.fps).unwrap_or(30),
        options.ecc_symbols.or(ctx.config.ecc_symbols).unwrap_or(32)
    );
    println!(
        "{:<22} {:<15} {:>7} {:>9} {:>10} {:>9} {:>10} {:>10} {:>7}",
        "Backend",
        "Mode",
        "Frames",
        "Enc time",
        "Enc speed",
        "Dec time",
        "Dec speed",
        "MP4",
        "Result"
    );
    let rows = run_matrix(tools, &payload, bytes, &options, ctx);
    as_table(&rows, bytes);
    calibration_from(diag::fingerprint(&tools.ffmpeg), &rows)
        .save(ctx.calibration)
        .with_context(|| format!("could not save {}", ctx.calibration.display()))?;
    eprintln!(
        "Calibration results saved to {}; encode with --optimise smallest|fastest|balanced to use them.",
        ctx.calibration.display()
    );
    Ok(())
}

// The estimate command never touches FFmpeg; encode and decode do.
fn uses_video(cli: &Cli) -> bool {
    !matches!(cli.command, Operation::Estimate { .. })
}

fn pick_ffmpeg(cli: &Cli, gpu: video::Gpu) -> PathBuf {
    if let Some(path) = &cli.ffmpeg {
        return path.clone();
    }
    if cli.no_download {
        return PathBuf::from(video::default_ffmpeg());
    }
    let default = PathBuf::from(video::default_ffmpeg());
    let full_build = download::ffmpeg_works(&default)
        && (gpu == video::Gpu::Off || download::has_hardware_encoders(&default));
    if full_build {
        return default;
    }
    if !uses_video(cli) {
        // Estimation must never download; an existing cached build still
        // matches the fingerprint of a real encode run.
        return download::cached().unwrap_or(default);
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
    let config = Config::load(cli.config.as_deref())?;
    let threads = cli.threads.or(config.threads).unwrap_or(0);
    let calibration = cli
        .calibration
        .clone()
        .unwrap_or_else(|| PathBuf::from(calibrate::FILE_NAME));
    let ctx = Ctx {
        config: &config,
        calibration: &calibration,
        quiet: false,
    };
    let with_video = uses_video(&cli);
    let tools = video::Tools {
        ffmpeg: pick_ffmpeg(&cli, cli.gpu.or(config.gpu).unwrap_or(video::Gpu::Auto)),
        ffprobe: cli.ffprobe,
        threads,
        gpu: cli.gpu.or(config.gpu).unwrap_or(video::Gpu::Auto),
    };
    if threads != 0 {
        rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
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
            no_password,
            password,
            settings,
            stealth,
            stealth_block,
            stealth_intensity,
            stealth_dense,
            stealth_no_audio,
        } => {
            let frames = match stealth {
                Some(background) => encode_stealth(
                    &tools,
                    &input,
                    &output,
                    &background,
                    password,
                    no_password,
                    stealth_block,
                    stealth_intensity,
                    stealth_dense,
                    stealth_no_audio,
                    settings,
                    &ctx,
                )?,
                None => encode(
                    &tools,
                    &input,
                    &output,
                    password,
                    no_password,
                    settings,
                    &ctx,
                )?,
            };
            eprintln!(
                "Wrote {} frames in {} in {:.2}s.",
                frames,
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
                &password,
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
        Operation::Estimate { input, settings } => estimate(&tools, &input, settings, &ctx)?,
        Operation::Benchmark {
            input,
            size,
            settings,
        } => benchmark(&tools, input, size, settings, &ctx)?,
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
