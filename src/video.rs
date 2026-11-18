use crate::format::{BLOCK_SIZES, Grid, TRANSPORT_SIZE, parse_transport, validate_resolution};
use anyhow::{Context, Result, ensure};
use std::ffi::OsStr;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Instant;

pub fn local_path(path: &Path) -> PathBuf {
    // Force FFmpeg's local-file protocol, including paths containing colons.
    let mut url = std::ffi::OsString::from("file:");
    url.push(path);
    url.into()
}

pub struct Tools {
    pub ffmpeg: PathBuf,
    pub ffprobe: Option<PathBuf>,
    pub threads: usize,
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
        let small = format!("{}x{}", grid.width / grid.block, grid.height / grid.block);
        let scale = format!(
            "scale={}:{}:flags=neighbor,setsar=1",
            grid.width, grid.height
        );
        let mut cmd = self.command();
        cmd.args([
            "-y",
            "-f",
            "rawvideo",
            "-pixel_format",
            "gray",
            "-video_size",
        ])
        .arg(small)
        .args([
            "-framerate",
            &settings.fps.to_string(),
            "-i",
            "pipe:0",
            "-an",
            "-filter_threads",
            "1",
            "-vf",
            &scale,
            "-c:v",
            "libx264",
            "-preset",
            &settings.preset,
            "-crf",
            &settings.crf.to_string(),
            "-threads:v",
            &self.threads.to_string(),
            "-pix_fmt",
            "yuv420p",
            "-f",
            "mp4",
        ])
        .arg(local_path(output))
        .stdin(Stdio::piped())
        .stdout(Stdio::null());
        self.spawn(cmd)
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
            "gray",
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
        let child = cmd.spawn().with_context(|| {
            format!(
                "could not run {} (install FFmpeg with libx264 or pass --ffmpeg)",
                self.ffmpeg.display()
            )
        })?;
        Ok(Process {
            child,
            waited: false,
        })
    }
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

pub struct Settings {
    pub grid: Grid,
    pub fps: u8,
    pub crf: u8,
    pub preset: String,
}

pub struct Process {
    pub child: Child,
    waited: bool,
}

impl Process {
    pub fn finish(mut self) -> Result<()> {
        let status = self.child.wait()?;
        self.waited = true;
        ensure!(status.success(), "FFmpeg failed ({status})");
        Ok(())
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        if !self.waited {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

pub struct Progress {
    label: &'static str,
    interval: Instant,
    enabled: bool,
    total: u64,
}

impl Progress {
    pub fn new(label: &'static str, total: u64) -> Self {
        use std::io::IsTerminal;
        Self {
            label,
            interval: Instant::now(),
            enabled: io::stderr().is_terminal(),
            total,
        }
    }

    pub fn update(&mut self, n: u64) {
        if self.enabled && self.interval.elapsed().as_secs() >= 2 {
            eprintln!(
                "{}: {n}/{} frames ({:.1}%)",
                self.label,
                self.total,
                (n as f64 / self.total as f64) * 100.0
            );
            self.interval = Instant::now();
        }
    }
}

const fn expansion() -> [[u8; 8]; 256] {
    let mut lut = [[0; 8]; 256];
    let mut value = 0;
    while value < 256 {
        let mut bit = 0;
        while bit < 8 {
            lut[value][bit] = if value & (128 >> bit) != 0 { 255 } else { 0 };
            bit += 1;
        }
        value += 1;
    }
    lut
}
static EXPAND: [[u8; 8]; 256] = expansion();

pub struct Sink<W> {
    pipe: W,
    grid: Vec<u8>,
    pos: usize,
    sent: u64,
    progress: Progress,
}

impl<W: Write> Sink<W> {
    pub fn new(pipe: W, grid: Grid, frames: u64) -> Self {
        Self {
            pipe,
            grid: vec![0; grid.bits()],
            pos: 0,
            sent: 0,
            progress: Progress::new("Encode", frames),
        }
    }

    pub fn push(&mut self, mut bytes: &[u8]) -> Result<()> {
        while !bytes.is_empty() {
            let n = bytes.len().min((self.grid.len() - self.pos) / 8);
            let end = self.pos + n * 8;
            for (dst, &byte) in self.grid[self.pos..end]
                .as_chunks_mut::<8>()
                .0
                .iter_mut()
                .zip(bytes)
            {
                *dst = EXPAND[byte as usize];
            }
            self.pos = end;
            bytes = &bytes[n..];
            if self.pos == self.grid.len() {
                self.flush_frame()?;
            } else if !bytes.is_empty() {
                // A byte may straddle frames with a non-byte-aligned grid.
                let tail = self.grid.len() - self.pos;
                let bits = &EXPAND[bytes[0] as usize];
                self.grid[self.pos..].copy_from_slice(&bits[..tail]);
                self.flush_frame()?;
                self.grid[..8 - tail].copy_from_slice(&bits[tail..]);
                self.pos = 8 - tail;
                bytes = &bytes[1..];
            }
        }
        Ok(())
    }

    fn flush_frame(&mut self) -> Result<()> {
        self.pipe
            .write_all(&self.grid)
            .context("could not send frame to FFmpeg")?;
        self.pos = 0;
        self.sent += 1;
        self.progress.update(self.sent);
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

fn sampler(block: usize) -> Sampler {
    match block {
        8 => sample::<8>,
        10 => sample::<10>,
        12 => sample::<12>,
        16 => sample::<16>,
        20 => sample::<20>,
        _ => unreachable!(),
    }
}

fn sample<const B: usize>(frame: &[u8], grid: Grid, bits: &mut [u8]) {
    let quarter = B / 4;
    let center = B - 2 * quarter;
    let cells = grid.width / B;
    for (y, row) in bits.chunks_exact_mut(cells).enumerate() {
        let start = (y * B + quarter) * grid.width + quarter;
        for (x, bit) in row.iter_mut().enumerate() {
            let mut sum = 0u32;
            let origin = start + x * B;
            for dy in 0..center {
                let offset = origin + dy * grid.width;
                for &value in &frame[offset..offset + center] {
                    sum += u32::from(value);
                }
            }
            *bit = u8::from(sum >= (128 * center * center) as u32);
        }
    }
}

fn pack(bits: &[u8; 8]) -> u8 {
    (bits[0] << 7)
        | (bits[1] << 6)
        | (bits[2] << 5)
        | (bits[3] << 4)
        | (bits[4] << 3)
        | (bits[5] << 2)
        | (bits[6] << 1)
        | bits[7]
}

pub struct Source<R> {
    pipe: R,
    image: Vec<u8>,
    bits: Vec<u8>,
    pos: usize,
    grid: Grid,
    sampler: Sampler,
    read: u64,
    progress: Progress,
}

impl<R: Read> Source<R> {
    pub fn open(
        mut pipe: R,
        width: usize,
        height: usize,
        block: Option<usize>,
    ) -> Result<(Self, usize, u64)> {
        let mut image = vec![0; width * height];
        pipe.read_exact(&mut image)
            .context("no complete first frame from FFmpeg")?;
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
            let mut bits = vec![0u8; grid.bits()];
            sample(&image, grid, &mut bits);
            for (out, b) in head.iter_mut().zip(bits.as_chunks::<8>().0) {
                *out = pack(b);
            }
            if let Ok((parity, len)) = parse_transport(&head, grid) {
                return Ok((
                    Self {
                        pipe,
                        image,
                        bits,
                        pos: TRANSPORT_SIZE * 8,
                        grid,
                        sampler: sample,
                        read: 1,
                        progress: Progress::new("Decode", 1),
                    },
                    parity,
                    len,
                ));
            }
        }
        anyhow::bail!("no Bit2Vid v1 header found; wrong layout or damaged first frame")
    }

    pub fn set_frames(&mut self, expected: u64) {
        self.progress.total = expected;
    }

    pub fn block(&self) -> usize {
        self.grid.block
    }

    fn next_frame(&mut self) -> io::Result<()> {
        self.pipe.read_exact(&mut self.image)?;
        (self.sampler)(&self.image, self.grid, &mut self.bits);
        self.read += 1;
        self.progress.update(self.read);
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
        if self.pos == self.bits.len() {
            self.next_frame()?;
        }
        let n = out.len().min((self.bits.len() - self.pos) / 8);
        if n != 0 {
            let end = self.pos + n * 8;
            for (byte, bits) in out
                .iter_mut()
                .zip(self.bits[self.pos..end].as_chunks::<8>().0)
            {
                *byte = pack(bits);
            }
            self.pos = end;
            return Ok(n);
        }
        let mut bits = [0; 8];
        let tail = self.bits.len() - self.pos;
        bits[..tail].copy_from_slice(&self.bits[self.pos..]);
        self.next_frame()?;
        bits[tail..].copy_from_slice(&self.bits[..8 - tail]);
        self.pos = 8 - tail;
        out[0] = pack(&bits);
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
    use super::frame_sync_for;

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
}
