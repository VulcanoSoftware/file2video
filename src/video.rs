use crate::format::{BLOCK_SIZES, Grid, TRANSPORT_SIZE, parse_transport, validate_resolution};
use anyhow::{Context, Result, bail, ensure};
use clap::ValueEnum;
use std::ffi::OsStr;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Instant;

// A higher CRF erases whole single-pixel luma levels per frame; crf 8 keeps
// the residual cell noise far below the Reed-Solomon correction budget of
// every supported parity count while still letting the encoder do most of
// the compression work.
const COMPACT_MAX_CRF: u8 = 8;
// All-intra frames get no relief from temporal prediction, and crf 8 damages
// 10x more cells than crf 6 while the video only shrinks by 8%, so the intra
// mode holds one CRF step in reserve.
const INTRA_MAX_CRF: u8 = 6;

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum Gpu {
    /// Pick the first working hardware encoder, or fall back to CPU.
    Auto,
    /// Always encode with CPU libx264.
    Off,
    /// AMD AMF (Windows).
    Amf,
    /// NVIDIA NVENC (Windows/Linux).
    Nvenc,
    /// Intel Quick Sync (Windows/Linux).
    Qsv,
    /// VA-API (Linux).
    Vaapi,
    /// VideoToolbox (macOS).
    VideoToolbox,
}

impl Gpu {
    const VAAPI_DEVICE: &'static str = "/dev/dri/renderD128";

    pub fn encoder_name(self) -> &'static str {
        match self {
            Self::Amf => "h264_amf",
            Self::Nvenc => "h264_nvenc",
            Self::Qsv => "h264_qsv",
            Self::Vaapi => "h264_vaapi",
            Self::VideoToolbox => "h264_videotoolbox",
            Self::Auto | Self::Off => unreachable!(),
        }
    }

    /// The short name used in the calibration file and the config file.
    pub fn short_name(self) -> &'static str {
        match self {
            Self::Amf => "amf",
            Self::Nvenc => "nvenc",
            Self::Qsv => "qsv",
            Self::Vaapi => "vaapi",
            Self::VideoToolbox => "video-toolbox",
            Self::Auto => "auto",
            Self::Off => "cpu",
        }
    }

    /// Parses the short name; the inverse of `short_name`.
    pub fn from_name(name: &str) -> Option<Self> {
        Some(match name.trim().to_ascii_lowercase().as_str() {
            "cpu" | "libx264" | "x264" => Self::Off,
            "amf" => Self::Amf,
            "nvenc" => Self::Nvenc,
            "qsv" => Self::Qsv,
            "vaapi" => Self::Vaapi,
            "video-toolbox" | "videotoolbox" => Self::VideoToolbox,
            "auto" => Self::Auto,
            _ => return None,
        })
    }

    fn candidates() -> &'static [Self] {
        if cfg!(windows) {
            &[Self::Amf, Self::Nvenc, Self::Qsv]
        } else if cfg!(target_os = "macos") {
            &[Self::VideoToolbox]
        } else {
            &[Self::Vaapi, Self::Nvenc, Self::Qsv]
        }
    }

    // On hybrid-laptop GPUs (an integrated GPU plus a discrete one) the
    // discrete GPU is usually much faster, so put its vendor's backend first
    // when the OS reports such a card.
    pub fn prioritized_candidates(gpus: &[String]) -> Vec<Self> {
        let mut candidates = Self::candidates().to_vec();
        let discrete = gpus.iter().find_map(|name| {
            let lower = name.to_ascii_lowercase();
            if lower.contains("nvidia") || lower.contains("geforce") {
                Some(Self::Nvenc)
            } else if lower.contains("intel") && lower.contains("arc") {
                Some(Self::Qsv)
            } else if (lower.contains("radeon") || lower.contains("amd")) && lower.contains("rx") {
                Some(Self::Amf)
            } else {
                None
            }
        });
        if let Some(index) = discrete.and_then(|preferred| {
            candidates
                .iter()
                .position(|candidate| *candidate == preferred)
        }) {
            let preferred = candidates.remove(index);
            candidates.insert(0, preferred);
        }
        candidates
    }

    fn device_args(self) -> Vec<String> {
        if self == Self::Vaapi {
            vec![
                "-init_hw_device".to_owned(),
                format!("vaapi=hw:{}", Self::VAAPI_DEVICE),
            ]
        } else {
            Vec::new()
        }
    }

    fn filter_chain(self, scale: &str) -> String {
        if self == Self::Vaapi {
            format!("{scale},format=nv12,hwupload")
        } else {
            scale.to_owned()
        }
    }

    fn passes_pix_fmt(self) -> bool {
        self != Self::Vaapi
    }

    fn quality_args(self, crf: u8, preset: &str) -> Vec<String> {
        let mut args = Vec::new();
        match self {
            Self::Amf => {
                args.push("-quality".into());
                args.push(amf_quality(preset).into());
                args.push("-rc".into());
                args.push("cqp".into());
                for option in ["-qp_i", "-qp_p", "-qp_b"] {
                    args.push(option.into());
                    args.push(crf.to_string());
                }
            }
            Self::Nvenc => {
                args.push("-preset".into());
                args.push(nvenc_preset(preset).into());
                args.extend(["-rc".into(), "vbr".into()]);
                args.push("-cq".into());
                args.push(crf.to_string());
                args.extend(["-b:v".into(), "0".into()]);
            }
            Self::Qsv => {
                args.push("-preset".into());
                args.push(preset.into());
                args.push("-global_quality".into());
                args.push(crf.to_string());
            }
            Self::Vaapi => {
                args.extend(["-rc_mode".into(), "CQP".into()]);
                args.push("-qp".into());
                args.push(crf.to_string());
            }
            Self::VideoToolbox => {
                args.push("-q:v".into());
                args.push(vt_quality(crf).to_string());
            }
            Self::Auto | Self::Off => unreachable!(),
        }
        args
    }
}

pub fn local_path(path: &Path) -> PathBuf {
    // Force FFmpeg's local-file protocol, including paths containing colons.
    let mut url = std::ffi::OsString::from("file:");
    url.push(path);
    url.into()
}

#[derive(Clone)]
pub struct Tools {
    pub ffmpeg: PathBuf,
    pub ffprobe: Option<PathBuf>,
    pub threads: usize,
    pub gpu: Gpu,
}

impl Tools {
    fn probe_path(&self) -> PathBuf {
        self.ffprobe.clone().unwrap_or_else(|| {
            self.ffmpeg.with_file_name(if cfg!(windows) {
                "ffprobe.exe"
            } else {
                "ffprobe"
            })
        })
    }

    pub fn dimensions(&self, input: &Path) -> Result<(usize, usize)> {
        let probe = self.probe_path();
        let out = Command::new(&probe)
            .args([
                "-v",
                "error",
                "-protocol_whitelist",
                "file",
                "-f",
                "mov",
                "-select_streams",
                "v:0",
                "-show_entries",
                "stream=width,height",
                "-of",
                "csv=p=0:s=x",
            ])
            .arg(local_path(input))
            .stdin(Stdio::null())
            .stderr(Stdio::inherit())
            .output()
            .with_context(|| {
                format!(
                    "could not run {} (install FFmpeg with ffprobe or pass --ffprobe)",
                    probe.display()
                )
            })?;
        ensure!(out.status.success(), "ffprobe failed ({})", out.status);
        let text = std::str::from_utf8(&out.stdout)?.trim();
        let (w, h) = text.split_once('x').context("video dimensions not found")?;
        let dimensions = (w.parse()?, h.parse()?);
        validate_resolution(dimensions.0, dimensions.1)?;
        Ok(dimensions)
    }

    pub fn encode(&self, settings: &Settings, output: &Path) -> Result<Process> {
        let grid = settings.grid;
        let small = if settings.compact {
            format!("{}x{}", grid.width, grid.height)
        } else {
            format!("{}x{}", grid.width / grid.block, grid.height / grid.block)
        };
        let scale = if settings.compact {
            "null".to_owned()
        } else {
            format!(
                "scale={}:{}:flags=neighbor,setsar=1",
                grid.width, grid.height
            )
        };
        // One cell is one video pixel; even sparse quantization noise breaks
        // a cell value, so compact mode caps every quality setting at a level
        // whose Reed-Solomon overhead stays comfortably below the correction
        // capacity of the frame, regardless of what the caller passed.
        let crf = if settings.compact {
            settings.crf.min(if settings.intra {
                INTRA_MAX_CRF
            } else {
                COMPACT_MAX_CRF
            })
        } else {
            settings.crf
        };
        let encoder = self.select_encoder(crf)?;
        let mut cmd = self.command();
        if let Encoder::Gpu(backend) = encoder {
            for arg in backend.device_args() {
                cmd.arg(arg);
            }
        }
        cmd.arg("-y")
            .args(["-f", "rawvideo", "-pixel_format"])
            .arg(if settings.chroma { "yuv420p" } else { "gray" })
            .arg("-video_size")
            .arg(&small)
            .args([
                "-framerate",
                &settings.fps.to_string(),
                "-i",
                "pipe:0",
                "-an",
                "-filter_threads",
                "1",
            ]);
        if settings.intra {
            cmd.args(["-g", "1"]);
        }
        cmd.arg("-vf");
        let quality: Vec<String>;
        match encoder {
            Encoder::Cpu => {
                cmd.arg(&scale)
                    .args(["-c:v", "libx264", "-preset", &settings.preset])
                    .args(["-crf", &crf.to_string()])
                    .args(["-threads:v", &self.threads.to_string()])
                    .args(["-pix_fmt", "yuv420p", "-f", "mp4"]);
            }
            Encoder::Gpu(backend) => {
                cmd.arg(backend.filter_chain(&scale))
                    .args(["-c:v", backend.encoder_name()]);
                quality = backend.quality_args(crf, &settings.preset);
                cmd.args(&quality);
                if backend.passes_pix_fmt() {
                    cmd.args(["-pix_fmt", "yuv420p"]);
                }
                cmd.args(["-f", "mp4"]);
            }
        }
        cmd.arg(local_path(output))
            .stdin(Stdio::piped())
            .stdout(Stdio::null());
        self.spawn(cmd)
    }

    // Hardware encoder candidates this FFmpeg build could run, discrete GPUs first.
    pub fn available_backends(&self) -> Vec<Gpu> {
        let Ok(list) = self.encoder_list() else {
            return Vec::new();
        };
        let gpus = crate::diag::gpu_names();
        Gpu::prioritized_candidates(&gpus)
            .into_iter()
            .filter(|backend| {
                list.split_whitespace()
                    .any(|word| word == backend.encoder_name())
            })
            .collect()
    }

    fn encoder_list(&self) -> Result<String> {
        let out = Command::new(&self.ffmpeg)
            .args(["-hide_banner", "-encoders"])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .with_context(|| format!("could not run {}", self.ffmpeg.display()))?;
        ensure!(
            out.status.success(),
            "FFmpeg encoder list failed ({})",
            out.status
        );
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    // A tiny trial encode: hardware encoders can be listed yet still fail
    // without a usable device. The source must be large enough for every
    // encoder's minimum frame size (AMF rejects 64x64 with AMF_OUT_OF_RANGE)
    // and emit more frames than the encoder buffers internally, or the null
    // muxer receives no packets and FFmpeg aborts with an unrelated error.
    fn preflight(&self, backend: Gpu, crf: u8) -> Result<(), String> {
        let mut cmd = Command::new(&self.ffmpeg);
        cmd.args(["-hide_banner", "-loglevel", "error", "-nostdin"])
            .stdin(Stdio::null());
        for arg in backend.device_args() {
            cmd.arg(arg);
        }
        cmd.args(["-f", "lavfi", "-i", "color=c=black:s=256x256:r=30:d=1"])
            .arg("-vf");
        if backend.passes_pix_fmt() {
            cmd.arg("format=yuv420p");
        } else {
            cmd.arg("format=nv12,hwupload");
        }
        cmd.args(["-c:v", backend.encoder_name()]);
        let args = backend.quality_args(crf, "slow");
        for arg in &args {
            cmd.arg(arg);
        }
        cmd.args(["-f", "null", "-"]);
        if debug() {
            let quoted: Vec<String> = cmd
                .get_args()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect();
            eprintln!(
                "Trial encode with {}: {} {}",
                backend.encoder_name(),
                cmd.get_program().to_string_lossy(),
                quoted.join(" ")
            );
        }
        match cmd.stderr(Stdio::piped()).spawn() {
            Ok(mut child) => {
                let mut pipe = child.stderr.take().unwrap();
                let mut text = String::new();
                let status = child.wait();
                let _ = pipe.read_to_string(&mut text);
                match status {
                    Ok(status) if status.success() => Ok(()),
                    Ok(status) => Err(trial_failure(&text, &status.to_string())),
                    Err(error) => Err(trial_failure(&text, &error.to_string())),
                }
            }
            Err(error) => Err(trial_failure("", &error.to_string())),
        }
    }

    fn select_encoder(&self, crf: u8) -> Result<Encoder> {
        if self.gpu == Gpu::Off {
            return Ok(Encoder::Cpu);
        }
        let forced = self.gpu != Gpu::Auto && self.gpu != Gpu::Off;
        let list = self.encoder_list()?;
        let candidates = if forced {
            vec![self.gpu]
        } else {
            let gpus = crate::diag::gpu_names();
            let ordered = Gpu::prioritized_candidates(&gpus);
            if ordered.first() != Gpu::candidates().first() && ordered.len() > 1 {
                eprintln!(
                    "Detected a discrete GPU; trying {} first.",
                    ordered[0].encoder_name()
                );
            }
            if debug() {
                let names: Vec<&str> = ordered.iter().map(|g| g.encoder_name()).collect();
                eprintln!("GPU encoder order: {}.", names.join(", "));
            }
            ordered
        };
        for backend in candidates {
            let name = backend.encoder_name();
            if !list.split_whitespace().any(|word| word == name) {
                if forced {
                    bail!(
                        "{name} is not part of this FFmpeg build; pass --gpu off or install an FFmpeg with it"
                    );
                }
                eprintln!("GPU encoder {name} is not in this FFmpeg build; skipping.");
                continue;
            }
            match self.preflight(backend, crf) {
                Ok(()) => {
                    eprintln!("Encoding with {name} on the GPU.");
                    return Ok(Encoder::Gpu(backend));
                }
                Err(reason) if forced => bail!(
                    "{name} failed its trial encode: {reason}\nCheck your GPU driver, or pass --gpu off to encode on the CPU"
                ),
                Err(reason) => {
                    eprintln!(
                        "GPU encoder {name} failed its trial encode: {reason}; trying the next option."
                    );
                }
            }
        }
        if self.gpu == Gpu::Auto {
            eprintln!(
                "No usable GPU encoder found; falling back to CPU encoding (libx264).\nUpdate your GPU drivers so that the encoder becomes usable, or pass --gpu off to silence this."
            );
        }
        Ok(Encoder::Cpu)
    }

    pub fn decode(&self, input: &Path) -> Result<Process> {
        let frame_sync = self.frame_sync_args()?;
        let mut cmd = self.command();
        cmd.args([
            "-protocol_whitelist",
            "file",
            "-threads",
            &self.threads.to_string(),
            "-noautorotate",
            "-f",
            "mov",
            "-i",
        ])
        .arg(local_path(input))
        .args([
            "-map",
            "0:v:0",
            "-an",
            "-sn",
            "-dn",
            "-filter_threads",
            "1",
            frame_sync[0],
            frame_sync[1],
            "-threads:v",
            "1",
            "-f",
            "rawvideo",
            "-pix_fmt",
            "yuv420p",
            "pipe:1",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped());
        self.spawn(cmd)
    }

    // FFmpeg added -fps_mode in 5.1 and removed -vsync in 9. Avoid
    // duplicating or dropping frames to match timestamps on either version.
    fn frame_sync_args(&self) -> Result<[&'static str; 2]> {
        let out = Command::new(&self.ffmpeg)
            .arg("-version")
            .stdin(Stdio::null())
            .stderr(Stdio::inherit())
            .output()
            .with_context(|| {
                format!(
                    "could not run {} (install FFmpeg or pass --ffmpeg)",
                    self.ffmpeg.display()
                )
            })?;
        ensure!(
            out.status.success(),
            "FFmpeg version check failed ({})",
            out.status
        );
        Ok(frame_sync_for(&String::from_utf8_lossy(&out.stdout)))
    }

    fn command(&self) -> Command {
        let mut cmd = Command::new(&self.ffmpeg);
        cmd.args(["-hide_banner", "-loglevel", "error", "-nostdin"])
            .stderr(Stdio::inherit());
        cmd
    }

    fn spawn(&self, mut cmd: Command) -> Result<Process> {
        // Capture the child's stderr in a draining thread; keeping only the
        // last lines means a mid-run failure reports the actual FFmpeg error
        // instead of a bare broken pipe, without risking a full pipe buffer.
        const TAIL_BYTES: usize = 8192;
        let tail_shared = Arc::new(Mutex::new(Vec::new()));
        let mut child = cmd.stderr(Stdio::piped()).spawn().with_context(|| {
            format!(
                "could not run {} (install FFmpeg with libx264 or pass --ffmpeg)",
                self.ffmpeg.display()
            )
        })?;
        let pipe = child.stderr.take().context("missing FFmpeg stderr")?;
        let shared = Arc::clone(&tail_shared);
        let reader = std::thread::spawn(move || {
            let mut pipe = pipe;
            let mut buffer = [0u8; 4096];
            loop {
                match pipe.read(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let mut tail = shared
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner());
                        tail.extend_from_slice(&buffer[..n]);
                        if tail.len() > TAIL_BYTES {
                            let excess = tail.len() - TAIL_BYTES;
                            tail.drain(..excess);
                        }
                    }
                }
            }
        });
        Ok(Process {
            child,
            waited: false,
            tail: Some((tail_shared, reader)),
        })
    }
}

enum Encoder {
    Cpu,
    Gpu(Gpu),
}

impl Process {
    fn stderr_tail(&mut self) -> String {
        if let Some((shared, reader)) = self.tail.take() {
            let _ = reader.join();
            let tail = shared
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let text = String::from_utf8_lossy(tail.as_slice()).into_owned();
            let lines: Vec<&str> = text
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .collect();
            let start = lines.len().saturating_sub(4);
            return lines[start..].join("; ");
        }
        String::new()
    }

    pub fn finish(mut self) -> Result<()> {
        let status = self.child.wait()?;
        self.waited = true;
        if !status.success() {
            let tail = self.stderr_tail();
            if tail.is_empty() {
                bail!("FFmpeg failed ({status})");
            }
            bail!("FFmpeg failed ({status}): {tail}");
        }
        Ok(())
    }

    /// Attaches the captured FFmpeg output to an error that happened while
    /// the process was still running (for example a broken pipe after FFmpeg
    /// exited), then stops the process.
    pub fn explain(mut self, error: anyhow::Error) -> anyhow::Error {
        let _ = self.child.kill();
        let _ = self.child.wait();
        self.waited = true;
        let tail = self.stderr_tail();
        if tail.is_empty() {
            error
        } else {
            error.context(format!("FFmpeg reported: {tail}"))
        }
    }
}

fn debug() -> bool {
    std::env::var_os("FILE2VIDEO_DEBUG").is_some_and(|value| value != "0" && !value.is_empty())
}

fn trial_failure(stderr: &str, fallback: &str) -> String {
    let generic = |line: &str| {
        line.starts_with("[out#") || line.contains("Nothing was written into output file")
    };
    let lines: Vec<&str> = stderr
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !generic(line))
        .collect();
    if lines.is_empty() {
        return fallback.to_owned();
    }
    lines.join("; ")
}

fn amf_quality(preset: &str) -> &'static str {
    match preset {
        "ultrafast" | "superfast" | "veryfast" | "faster" => "speed",
        "fast" | "medium" => "balanced",
        _ => "quality",
    }
}

fn nvenc_preset(preset: &str) -> &'static str {
    match preset {
        "ultrafast" => "p1",
        "superfast" => "p2",
        "veryfast" => "p3",
        "faster" | "fast" => "p4",
        "medium" => "p5",
        "slow" | "slower" => "p6",
        _ => "p7",
    }
}

fn vt_quality(crf: u8) -> u8 {
    100u8.saturating_sub(crf)
}

fn frame_sync_for(version_line: &str) -> [&'static str; 2] {
    let version = version_line
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(2));
    let version = version.and_then(|v| {
        let mut parts = v.trim_start_matches('n').split('.');
        let major: u32 = parts.next()?.parse().ok()?;
        let minor = parts
            .next()
            .and_then(|s| s.split('-').next())
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        Some((major, minor))
    });
    if version.is_some_and(|v| v < (5, 1)) {
        ["-vsync", "0"]
    } else {
        ["-fps_mode", "passthrough"]
    }
}

#[derive(Clone)]
pub struct Settings {
    /// The cell grid: one cell per frame fed to FFmpeg. In compact mode a
    /// cell is one video pixel; otherwise a cell is a grid.block pixel square.
    pub grid: Grid,
    /// The nominal grid stored in the transport header.
    pub display: Grid,
    pub fps: u8,
    pub crf: u8,
    pub preset: String,
    /// Luma levels per cell: 4 (16 levels).
    pub cell_bits: u8,
    /// Store the video at cell resolution instead of full resolution.
    pub compact: bool,
    /// Store payload cells in the U and V chroma planes as well (+50% capacity).
    pub chroma: bool,
    /// Encode every frame as an all-intra keyframe.
    pub intra: bool,
}

type Tail = (Arc<Mutex<Vec<u8>>>, JoinHandle<()>);

pub struct Process {
    pub child: Child,
    waited: bool,
    tail: Option<Tail>,
}

impl Drop for Process {
    fn drop(&mut self) {
        if !self.waited {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
        if let Some((_, reader)) = self.tail.take() {
            let _ = reader.join();
        }
    }
}

pub struct Progress {
    label: &'static str,
    started: Instant,
    interval: Instant,
    enabled: bool,
    total: u64,
}

impl Progress {
    pub fn new(label: &'static str, total: u64) -> Self {
        use std::io::IsTerminal;
        Self {
            label,
            started: Instant::now(),
            interval: Instant::now(),
            enabled: io::stderr().is_terminal(),
            total,
        }
    }

    pub fn update(&mut self, n: u64, bytes: u64) {
        if self.enabled && self.interval.elapsed().as_secs() >= 2 {
            let elapsed = self.started.elapsed().as_secs_f64();
            let eta = if n > 0 && self.total > n {
                human_duration((elapsed / n as f64 * (self.total - n) as f64).ceil() as u64)
            } else {
                "?".to_owned()
            };
            eprintln!(
                "{}: {n}/{} frames ({:.1}%) | {} | elapsed {} | eta {}",
                self.label,
                self.total,
                (n as f64 / self.total as f64) * 100.0,
                speed(bytes, elapsed),
                human_duration(elapsed.ceil() as u64),
                eta
            );
            self.interval = Instant::now();
        }
    }
}

fn human_duration(seconds: u64) -> String {
    let (minutes, seconds) = (seconds / 60, seconds % 60);
    if minutes >= 60 {
        format!("{}h{:02}m{:02}s", minutes / 60, minutes % 60, seconds)
    } else if minutes > 0 {
        format!("{minutes}m{seconds:02}s")
    } else {
        format!("{seconds}s")
    }
}

pub fn speed(bytes: u64, seconds: f64) -> String {
    let rate = bytes as f64 / seconds / (1024.0 * 1024.0);
    format!("{rate:.1} MB/s")
}

const fn luma_levels() -> [u8; 16] {
    let mut levels = [0; 16];
    let mut level = 0;
    while level < 16 {
        levels[level] = (level * 17) as u8;
        level += 1;
    }
    levels
}
static LUMA_LEVELS: [u8; 16] = luma_levels();

pub struct Sink<W> {
    pipe: W,
    grid: Vec<u8>,
    pos: usize,
    sent: u64,
    bytes: u64,
    progress: Progress,
}

impl<W: Write> Sink<W> {
    // The frame buffer is a full Y plane followed, in chroma mode, by the U
    // and V planes; the payload nibbles flow through Y, U and V in that order.
    pub fn new(pipe: W, grid: Grid, chroma: bool, frames: u64) -> Self {
        let cells = grid.bits() + if chroma { grid.bits() / 2 } else { 0 };
        Self {
            pipe,
            grid: vec![0; cells],
            pos: 0,
            sent: 0,
            bytes: 0,
            progress: Progress::new("Encode", frames),
        }
    }

    pub fn push(&mut self, bytes: &[u8]) -> Result<()> {
        self.bytes += bytes.len() as u64;
        for &byte in bytes {
            self.place(byte >> 4)?;
            self.place(byte & 0x0f)?;
        }
        Ok(())
    }

    fn place(&mut self, nibble: u8) -> Result<()> {
        self.grid[self.pos] = LUMA_LEVELS[usize::from(nibble)];
        self.pos += 1;
        if self.pos == self.grid.len() {
            self.flush_frame()?;
        }
        Ok(())
    }

    fn flush_frame(&mut self) -> Result<()> {
        self.pipe
            .write_all(&self.grid)
            .context("could not send frame to FFmpeg")?;
        self.pos = 0;
        self.sent += 1;
        self.progress.update(self.sent, self.bytes);
        Ok(())
    }

    pub fn finish(mut self) -> Result<()> {
        if self.pos > 0 {
            self.grid[self.pos..].fill(0);
            self.flush_frame()?;
        }
        Ok(())
    }
}

type Sampler = fn(&[u8], Grid, &mut [u8]);
type Packer = fn(&[u8]) -> u8;

fn sampler(block: usize) -> Sampler {
    match block {
        1 => sample4::<1>,
        8 => sample4::<8>,
        10 => sample4::<10>,
        12 => sample4::<12>,
        16 => sample4::<16>,
        20 => sample4::<20>,
        _ => unreachable!(),
    }
}

fn sample4<const B: usize>(frame: &[u8], grid: Grid, cells: &mut [u8]) {
    let quarter = B / 4;
    let center = B - 2 * quarter;
    let columns = grid.width / B;
    for (y, row) in cells.chunks_exact_mut(columns).enumerate() {
        let start = (y * B + quarter) * grid.width + quarter;
        for (x, cell) in row.iter_mut().enumerate() {
            let mut sum = 0u32;
            let origin = start + x * B;
            for dy in 0..center {
                let offset = origin + dy * grid.width;
                for &value in &frame[offset..offset + center] {
                    sum += u32::from(value);
                }
            }
            *cell = (((sum / (center * center) as u32) + 8) / 17) as u8;
        }
    }
}

fn pack_nibbles(cells: &[u8]) -> u8 {
    (cells[0] << 4) | cells[1]
}

// Maps limited studio range (16..235) back to full range; matches the
// grayscale conversion FFmpeg applies down to the exact rounding.
fn unmap_studio_range(plane: &mut [u8]) {
    for value in plane.iter_mut() {
        let expanded = (u32::from(*value).saturating_sub(16) * 255 + 109) / 219;
        *value = expanded.min(255) as u8;
    }
}

pub struct Source<R> {
    pipe: R,
    image: Vec<u8>,
    units: Vec<u8>,
    pos: usize,
    grid: Grid,
    sampler: Sampler,
    packer: Packer,
    cell_bits: u8,
    compact: bool,
    chroma: bool,
    luma_cells: usize,
    read: u64,
    bytes: u64,
    progress: Progress,
}

impl<R: Read> Source<R> {
    pub fn open(
        mut pipe: R,
        width: usize,
        height: usize,
        block: Option<usize>,
    ) -> Result<(Self, usize, u64)> {
        // Always receive yuv420p frames; the Y plane carries the payload in
        // every mode, and the U and V planes carry more payload in chroma
        // mode. The Y-first plane order matches the pack order below.
        let luma_pixels = width * height;
        let quarter_plane = luma_pixels / 4;
        let mut image = vec![0; luma_pixels + 2 * quarter_plane];
        pipe.read_exact(&mut image)
            .context("no complete first frame from FFmpeg")?;
        // Non-chroma videos store a full-range gray frame, which FFmpeg maps
        // to the limited studio range when encoding. Recover it here so data
        // written by every released build keeps decoding correctly.
        let mut luma = image[..luma_pixels].to_vec();
        unmap_studio_range(&mut luma);
        let mut head = [0; TRANSPORT_SIZE];
        let blocks = block
            .as_ref()
            .map(std::slice::from_ref)
            .unwrap_or(&BLOCK_SIZES);
        for &block in blocks {
            let Ok(grid) = (Grid {
                width,
                height,
                block,
            })
            .validate() else {
                continue;
            };
            let sample = sampler(block);
            let mut cells = vec![0u8; grid.bits()];
            sample(&luma, grid, &mut cells);
            for (byte, unit) in head.iter_mut().zip(cells.as_chunks::<2>().0) {
                *byte = pack_nibbles(unit);
            }
            if let Ok(header) = parse_transport(&head)
                && !header.compact
                && header.width == width
                && header.height == height
                && header.block == block
                && header.cell_bits == 4
            {
                return Ok((
                    Self {
                        pipe,
                        image,
                        units: cells,
                        pos: TRANSPORT_SIZE * 2,
                        grid,
                        sampler: sample,
                        packer: pack_nibbles,
                        cell_bits: 4,
                        compact: false,
                        chroma: false,
                        luma_cells: grid.bits(),
                        read: 1,
                        bytes: 0,
                        progress: Progress::new("Decode", 1),
                    },
                    header.parity,
                    header.protected,
                ));
            }
        }
        if width * height >= TRANSPORT_SIZE * 2 {
            let grid = Grid {
                width,
                height,
                block: 1,
            };
            let sample = sampler(1);
            let chroma_grid = Grid {
                width: width / 2,
                height: height / 2,
                block: 1,
            };
            for chroma in [false, true] {
                let mut cells = vec![0u8; grid.bits() + if chroma { quarter_plane * 2 } else { 0 }];
                if chroma {
                    sample(&image[..luma_pixels], grid, &mut cells[..grid.bits()]);
                    sample4::<1>(
                        &image[luma_pixels..luma_pixels + quarter_plane],
                        chroma_grid,
                        &mut cells[grid.bits()..grid.bits() + quarter_plane],
                    );
                    sample4::<1>(
                        &image[luma_pixels + quarter_plane..],
                        chroma_grid,
                        &mut cells[grid.bits() + quarter_plane..],
                    );
                } else {
                    sample(&luma, grid, &mut cells);
                }
                for (byte, unit) in head.iter_mut().zip(cells.as_chunks::<2>().0) {
                    *byte = pack_nibbles(unit);
                }
                if let Ok(header) = parse_transport(&head)
                    && header.compact
                    && header.cell_bits == 4
                    && header.chroma == chroma
                {
                    return Ok((
                        Self {
                            pipe,
                            image,
                            units: cells,
                            pos: TRANSPORT_SIZE * 2,
                            grid,
                            sampler: sample,
                            packer: pack_nibbles,
                            cell_bits: 4,
                            compact: true,
                            chroma,
                            luma_cells: grid.bits(),
                            read: 1,
                            bytes: 0,
                            progress: Progress::new("Decode", 1),
                        },
                        header.parity,
                        header.protected,
                    ));
                }
            }
        }
        anyhow::bail!("not a file2video video; wrong layout or damaged first frame")
    }

    pub fn set_frames(&mut self, expected: u64) {
        self.progress.total = expected;
    }

    pub fn grid(&self) -> Grid {
        self.grid
    }

    pub fn cell_bits(&self) -> u8 {
        self.cell_bits
    }

    pub fn chroma(&self) -> bool {
        self.chroma
    }

    pub fn compact(&self) -> bool {
        self.compact
    }

    fn next_frame(&mut self) -> io::Result<()> {
        self.pipe.read_exact(&mut self.image)?;
        if !self.chroma {
            let luma_pixels = self.image.len() * 2 / 3;
            unmap_studio_range(&mut self.image[..luma_pixels]);
        }
        (self.sampler)(&self.image, self.grid, &mut self.units[..self.luma_cells]);
        if self.chroma {
            let luma = self.luma_cells;
            let quarter = luma / 4;
            let image = &self.image;
            let chroma_grid = Grid {
                width: self.grid.width / 2,
                height: self.grid.height / 2,
                block: 1,
            };
            sample4::<1>(
                &image[luma..luma + quarter],
                chroma_grid,
                &mut self.units[luma..luma + quarter],
            );
            sample4::<1>(
                &image[luma + quarter..luma + 2 * quarter],
                chroma_grid,
                &mut self.units[luma + quarter..luma + 2 * quarter],
            );
        }
        self.read += 1;
        self.bytes += self.units.len() as u64;
        self.progress.update(self.read, self.bytes);
        self.pos = 0;
        Ok(())
    }

    pub fn finish(mut self) -> Result<()> {
        let mut extra = [0; 1];
        ensure!(
            self.pipe.read(&mut extra)? == 0,
            "unexpected extra video frames"
        );
        Ok(())
    }
}

impl<R: Read> Read for Source<R> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        let units = usize::from(8 / self.cell_bits);
        let packer = self.packer;
        if self.pos == self.units.len() {
            self.next_frame()?;
        }
        let n = out.len().min((self.units.len() - self.pos) / units);
        if n != 0 {
            let end = self.pos + n * units;
            for (byte, chunk) in out
                .iter_mut()
                .zip(self.units[self.pos..end].chunks_exact(units))
            {
                *byte = packer(chunk);
            }
            self.pos = end;
            return Ok(n);
        }
        let mut chunk = [0; 8];
        let tail = self.units.len() - self.pos;
        chunk[..tail].copy_from_slice(&self.units[self.pos..]);
        self.next_frame()?;
        chunk[tail..units].copy_from_slice(&self.units[..units - tail]);
        self.pos = units - tail;
        out[0] = packer(&chunk[..units]);
        Ok(1)
    }
}

pub fn default_ffmpeg() -> &'static OsStr {
    OsStr::new(if cfg!(windows) {
        "ffmpeg.exe"
    } else {
        "ffmpeg"
    })
}

#[cfg(test)]
mod tests {
    use super::{
        Gpu, Sink, Source, amf_quality, frame_sync_for, human_duration, nvenc_preset, pack_nibbles,
        sampler, speed, trial_failure, vt_quality,
    };
    use crate::format::{self, Grid};
    use std::io::{Cursor, Read};

    #[test]
    fn packs_nibbles() {
        assert_eq!(pack_nibbles(&[12, 5]), 0xc5);
        assert_eq!(pack_nibbles(&[0, 15]), 0x0f);
    }

    // Emulates what FFmpeg does to a gray input frame: maps the full range
    // to the limited studio range and scales it up with nearest neighbor.
    fn emulate_ffmpeg_scale(raw: &[u8], grid: Grid) -> Vec<u8> {
        let frame_cells = grid.bits();
        let frame_pixels = grid.width * grid.height;
        let columns = grid.width / grid.block;
        let mut out = vec![0u8; raw.len() / frame_cells * frame_pixels];
        for (frame, small) in out
            .chunks_exact_mut(frame_pixels)
            .zip(raw.chunks_exact(frame_cells))
        {
            for (y, row) in frame.chunks_exact_mut(grid.width).enumerate() {
                for (x, pixel) in row.iter_mut().enumerate() {
                    *pixel = to_studio_range(small[(y / grid.block) * columns + x / grid.block]);
                }
            }
        }
        out
    }

    fn to_studio_range(value: u8) -> u8 {
        16 + ((u32::from(value) * 219 + 127) / 255) as u8
    }

    #[test]
    fn round_trips_16_level_full_resolution_frames() {
        let grid = Grid {
            width: 648,
            height: 360,
            block: 8,
        }
        .validate()
        .unwrap();
        let mut seed = 0x1234_abcdu32;
        let data: Vec<u8> = (0..4096)
            .map(|_| {
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                seed as u8
            })
            .collect();
        let mut raw = Vec::new();
        {
            let mut sink = Sink::new(&mut raw, grid, false, 1);
            sink.push(&format::transport(grid, 64, 285, 4, false, false))
                .unwrap();
            sink.push(&[7u8; format::ECC_SIZE]).unwrap();
            sink.push(&data).unwrap();
            sink.finish().unwrap();
        }
        let frame_pixels = grid.width * grid.height;
        let mut expanded = Vec::new();
        for frame in emulate_ffmpeg_scale(&raw, grid).chunks(frame_pixels) {
            expanded.extend_from_slice(frame);
            expanded.extend(std::iter::repeat_n(128u8, frame_pixels / 2));
        }
        let (mut source, parity, protected) =
            Source::open(Cursor::new(&expanded), 648, 360, None).unwrap();
        assert_eq!(parity, 64);
        assert_eq!(protected, 285);
        assert_eq!(source.cell_bits(), 4);
        let mut ecc = [0; 30];
        source.read_exact(&mut ecc).unwrap();
        let mut out = vec![0; data.len()];
        source.read_exact(&mut out).unwrap();
        assert_eq!(out, data);
        source.finish().unwrap();
    }

    #[test]
    fn round_trips_compact_one_pixel_cells() {
        let grid = Grid {
            width: 256,
            height: 256,
            block: 1,
        };
        let display = Grid {
            width: 648,
            height: 360,
            block: 8,
        };
        let mut seed = 0x9e37_79b9u32;
        let data: Vec<u8> = (0..4096)
            .map(|_| {
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                seed as u8
            })
            .collect();
        let mut raw = Vec::new();
        {
            let mut sink = Sink::new(&mut raw, grid, false, 1);
            sink.push(&format::transport(display, 64, 285, 4, true, false))
                .unwrap();
            sink.push(&[7u8; format::ECC_SIZE]).unwrap();
            sink.push(&data).unwrap();
            sink.finish().unwrap();
        }
        raw.extend(std::iter::repeat_n(128u8, 256 * 256 / 2));
        for pixel in &mut raw[..256 * 256] {
            *pixel = to_studio_range(*pixel);
        }
        let (mut source, parity, protected) =
            Source::open(Cursor::new(&raw), 256, 256, None).unwrap();
        assert_eq!(parity, 64);
        assert_eq!(protected, 285);
        assert_eq!(source.cell_bits(), 4);
        assert!(!source.chroma());
        let mut ecc = [0; format::ECC_SIZE];
        source.read_exact(&mut ecc).unwrap();
        let mut out = vec![0; data.len()];
        source.read_exact(&mut out).unwrap();
        assert_eq!(out, data);
        source.finish().unwrap();
    }

    #[test]
    fn round_trips_compact_chroma_cells() {
        let grid = Grid {
            width: 256,
            height: 256,
            block: 1,
        };
        let display = Grid {
            width: 648,
            height: 360,
            block: 8,
        };
        let mut seed = 0x1f2e_3d4cu32;
        let data: Vec<u8> = (0..4096)
            .map(|_| {
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                seed as u8
            })
            .collect();
        let mut raw = Vec::new();
        {
            let mut sink = Sink::new(&mut raw, grid, true, 1);
            sink.push(&format::transport(display, 64, 285, 4, true, true))
                .unwrap();
            sink.push(&[7u8; format::ECC_SIZE]).unwrap();
            sink.push(&data).unwrap();
            sink.finish().unwrap();
        }
        let (mut source, parity, protected) =
            Source::open(Cursor::new(&raw), 256, 256, None).unwrap();
        assert_eq!(parity, 64);
        assert_eq!(protected, 285);
        assert_eq!(source.cell_bits(), 4);
        assert!(source.chroma());
        let mut ecc = [0; format::ECC_SIZE];
        source.read_exact(&mut ecc).unwrap();
        let mut out = vec![0; data.len()];
        source.read_exact(&mut out).unwrap();
        assert_eq!(out, data);
        source.finish().unwrap();
    }

    #[test]
    fn samples_luma_levels_back_to_nibbles() {
        let grid = Grid {
            width: 648,
            height: 360,
            block: 8,
        }
        .validate()
        .unwrap();
        let frame = vec![17u8; grid.width * grid.height];
        let mut cells = vec![0u8; grid.bits()];
        sampler(8)(&frame, grid, &mut cells);
        assert_eq!(cells[0], 1);
        assert!(cells[..grid.bits()].iter().all(|&cell| cell == 1));
    }

    #[test]
    fn chooses_supported_frame_sync_option() {
        for version in ["4.4.2-0ubuntu0.22.04.1", "5.0-static"] {
            assert_eq!(
                frame_sync_for(&format!("ffmpeg version {version} Copyright")),
                ["-vsync", "0"]
            );
        }
        for version in ["5.1.6", "9.0.2", "n8.0.1", "N-123456-g12345"] {
            assert_eq!(
                frame_sync_for(&format!("ffmpeg version {version} Copyright")),
                ["-fps_mode", "passthrough"]
            );
        }
    }

    #[test]
    fn hybrid_laptops_prefer_the_discrete_gpu() {
        if cfg!(target_os = "macos") {
            return;
        }
        let hybrid = vec![
            "AMD Radeon(TM) 760M Graphics".to_owned(),
            "NVIDIA GeForce RTX 4060 Laptop GPU".to_owned(),
        ];
        assert_eq!(Gpu::prioritized_candidates(&hybrid)[0], Gpu::Nvenc);
        assert_eq!(
            Gpu::prioritized_candidates(&["Intel(R) Arc(TM) Graphics".to_owned()])[0],
            Gpu::Qsv
        );
        let integrated = vec!["AMD Radeon(TM) Graphics".to_owned()];
        assert_eq!(
            Gpu::prioritized_candidates(&integrated),
            Gpu::candidates().to_vec()
        );
        assert_eq!(Gpu::prioritized_candidates(&[]), Gpu::candidates().to_vec());
    }

    #[test]
    fn formats_durations_and_speeds() {
        assert_eq!(human_duration(45), "45s");
        assert_eq!(human_duration(65), "1m05s");
        assert_eq!(human_duration(3615), "1h00m15s");
        assert_eq!(speed(150 * 1024 * 1024, 1.5), "100.0 MB/s");
        assert_eq!(speed(0, 1.5), "0.0 MB/s");
    }

    #[test]
    fn maps_presets_and_quality_to_gpu_encoders() {
        assert_eq!(nvenc_preset("slow"), "p6");
        assert_eq!(nvenc_preset("veryfast"), "p3");
        assert_eq!(nvenc_preset("veryslow"), "p7");
        assert_eq!(amf_quality("slow"), "quality");
        assert_eq!(amf_quality("medium"), "balanced");
        assert_eq!(amf_quality("ultrafast"), "speed");
        assert_eq!(vt_quality(18), 82);
        assert_eq!(vt_quality(0), 100);
    }

    #[test]
    fn gpu_backends_have_expected_encoders() {
        assert_eq!(Gpu::Nvenc.encoder_name(), "h264_nvenc");
        assert_eq!(Gpu::Vaapi.encoder_name(), "h264_vaapi");
    }

    #[test]
    fn trial_failures_show_the_meaningful_error_line() {
        for stderr in ["", "  \n"] {
            assert_eq!(trial_failure(stderr, "exit status 1"), "exit status 1");
        }
        assert_eq!(
            trial_failure(
                "Warning: x\nError: amf init failed: AMF_NO_DLL\n",
                "exit status 1"
            ),
            "Warning: x; Error: amf init failed: AMF_NO_DLL"
        );
        assert_eq!(
            trial_failure(
                "[h264_amf @ 0x1] AMF_NO_DLL\n\
                 [out#0/null @ 0x2] Nothing was written into output file, because at least one of its streams received no packets.\n",
                "exit status 1"
            ),
            "[h264_amf @ 0x1] AMF_NO_DLL"
        );
        assert_eq!(
            trial_failure(
                "[out#0/null @ 0x2] Nothing was written into output file, because at least one of its streams received no packets.\n",
                "exit status 1"
            ),
            "exit status 1"
        );
    }
}
