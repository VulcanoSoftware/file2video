//! Stealth mode turns a file into a faint differential-luma data layer over
//! an existing background video (for example gameplay footage). The frames
//! look like the original video with a light digital noise filter, and the
//! decoder recovers the file blindly: it never needs the background video.
//!
//! Every frame keeps the background's chroma planes untouched and carries
//! data only in the luma plane. The frame is divided into square pixel
//! blocks (6x6 by default, `--stealth-block` overrides it); horizontally
//! adjacent block pairs store one bit by shifting their means
//! in opposite directions. A static checkerboard pattern along the frame
//! border gives the decoder exact grid alignment anchors. Anchors keep the
//! modulation inside the safe luma range: the background is clamped to
//! 55..=200 first, so every sample stays within 30..=225 after encoding.

use crate::ecc;
use crate::format::{
    ECC_SIZE, Grid, Layout, TRANSPORT_SIZE, Transport, parse_transport, transport,
    validate_resolution,
};
use crate::video::{Encoder, Progress, Tools, local_path};
use anyhow::{Context, Result, ensure};
use rayon::prelude::*;
use std::io::{Read, Write};
use std::path::Path;
use std::process::{ChildStdin, ChildStdout, Command, Stdio};
use tempfile::NamedTempFile;

/// Pixels per modulation block; every data bit occupies a pair of two
/// horizontally adjacent blocks. This is the default; `--stealth-block`
/// overrides it (smaller blocks carry more data per frame). Block 4 gets
/// a 1 GiB archive close to a regular encode's wall time; block 6 stays
/// available for maximum robustness through very aggressive re-encoding.
pub const BLOCK: usize = 4;
/// Even block sizes tried by the blind decoder, in probe order: the default
/// first, then the faster smaller blocks, then the sturdier larger ones.
const CANDIDATE_BLOCKS: [usize; 8] = [4, 6, 2, 8, 10, 12, 14, 16];
/// Luma delta applied to each block of a pair.
pub const MODULATION: i16 = 25;
/// The background luma is clamped to this range before modulation.
pub const BASE_LOW: u8 = 55;
pub const BASE_HIGH: u8 = 200;
/// Anchor rows along the top and bottom borders and anchor columns on both
/// side borders carry a static checkerboard pattern for grid alignment.
pub const ANCHOR_TOP_ROWS: usize = 3;
pub const ANCHOR_BOTTOM_ROWS: usize = 1;
pub const ANCHOR_COLS: usize = 1;
/// The luma the anchor checkerboard alternates around.
pub const ANCHOR_MID: u8 = 127;
/// Marks the start of the stealth byte stream (before the transport header).
pub const MARKER: [u8; 8] = [0xA5, 0x3C, 0x9B, 0x7E, 0x51, 0xF2, 0xC8, 0x40];
const HEADER_BYTES: usize = MARKER.len() + TRANSPORT_SIZE;
/// Minimum accepted anchor-contrast margin per block between the best and
/// second best alignment candidate.
const ANCHOR_MARGIN: u32 = 6;
/// The sum of a block that falls entirely outside of the frame; it keeps a
/// shifted candidate from winning the alignment score.
const DIFFERENT_SUM_SENTINEL: u64 = 0;

/// Fixed stealth-layer geometry derived from the output resolution and the
/// modulation block size.
#[derive(Clone, Copy, Debug)]
pub struct Geometry {
    pub width: usize,
    pub height: usize,
    pub block: usize,
    /// Bits carried by one block pair: 2 for the current quaternary
    /// modulation, 1 for the legacy binary layer (decode compatibility).
    pub symbol_bits: usize,
    columns: usize,
    rows: usize,
    pairs: usize,
    bits: usize,
}

impl Geometry {
    /// The current stealth layout: two modulation bits per block pair.
    pub fn new(width: usize, height: usize, block: usize) -> Result<Self> {
        Self::build(width, height, block, 2)
    }

    /// The pre-density-stealth layout: one modulation bit per block pair.
    /// Encoding no longer produces this; the decoder still probes for it so
    /// older videos keep restoring.
    pub fn legacy(width: usize, height: usize, block: usize) -> Result<Self> {
        Self::build(width, height, block, 1)
    }

    fn build(width: usize, height: usize, block: usize, symbol_bits: usize) -> Result<Self> {
        validate_resolution(width, height)?;
        ensure!(
            width.is_multiple_of(block) && height.is_multiple_of(block),
            "the stealth video resolution must be a multiple of {block} pixels"
        );
        let columns = width / block;
        let rows = height / block;
        ensure!(
            rows > ANCHOR_TOP_ROWS + ANCHOR_BOTTOM_ROWS && columns > 2 * ANCHOR_COLS,
            "the video is too small for the stealth anchor border"
        );
        let pairs = (columns - 2 * ANCHOR_COLS) / 2;
        ensure!(pairs > 0, "the video is too small to carry stealth data");
        let bits = pairs * (rows - ANCHOR_TOP_ROWS - ANCHOR_BOTTOM_ROWS) * symbol_bits / 8 * 8;
        ensure!(
            bits >= 8 * 32,
            "the video is too small to carry a stealth data layer"
        );
        Ok(Self {
            width,
            height,
            block,
            symbol_bits,
            columns,
            rows,
            pairs,
            bits,
        })
    }

    pub fn frame_bytes(&self) -> usize {
        self.bits / 8
    }

    fn is_anchor(&self, col: usize, row: usize) -> bool {
        row < ANCHOR_TOP_ROWS
            || row >= self.rows - ANCHOR_BOTTOM_ROWS
            || col < ANCHOR_COLS
            || col >= self.columns - ANCHOR_COLS
    }

    fn anchor_count(&self) -> usize {
        (0..self.rows)
            .map(|row| {
                (0..self.columns)
                    .filter(|&col| self.is_anchor(col, row))
                    .count()
            })
            .sum()
    }
}

/// Fixed stealth encoding settings resolved from the CLI and config file.
pub struct StealthSettings {
    pub width: usize,
    pub height: usize,
    /// The background is resampled to `width` x `height`; encoding skips the
    /// (identity) scale filter when the background already matches.
    pub rescale: bool,
    pub block: usize,
    pub fps: u8,
    pub crf: u8,
    pub preset: String,
    pub intra: bool,
    /// Reed-Solomon parity per 255-byte codeword. Stealth defaults to 96,
    /// roughly 38% redundancy, so heavy compression noise stays correctable.
    pub parity: usize,
}

fn anchor_value(col: usize, row: usize) -> u8 {
    if (col + row).is_multiple_of(2) {
        ANCHOR_MID + MODULATION as u8
    } else {
        ANCHOR_MID - MODULATION as u8
    }
}

fn fill_block(image: &mut [u8], block: usize, width: usize, x0: usize, y0: usize, value: u8) {
    for y in y0..y0 + block {
        let start = y * width + x0;
        image[start..start + block].fill(value);
    }
}

/// Per-pixel difference between a pair's outer modulation levels. The four
/// levels sit at ±(LEVEL_RANGE/3)/2 and ±(LEVEL_RANGE)/2 around the pair
/// mean, so the outer levels hit the same ±25-per-pixel swing the legacy
/// binary layer used, and the two added inner levels halve the noise
/// margin per symbol in exchange for double the data rate.
const LEVEL_RANGE: i64 = 50;

/// Sum distance between adjacent modulation levels for a block of `area`.
fn level_step(area: i64) -> i64 {
    area * LEVEL_RANGE / 3
}

/// Prepares one frame for encoding: clamps the background luma, stamps the
/// anchor checkerboard onto the border blocks, and applies the pair
/// modulation for `bits` (row-major, most significant bit first). Each pair
/// is modulated around its own mean so a symbol shifts both blocks in
/// opposite directions regardless of the background content; the decoder
/// recovers the symbol from the block-mean difference. Quaternary layers
/// carry two bits per pair across four luma levels; legacy layers one bit.
/// Data blocks past the end of `bits` keep the clamped background.
/// Bit `index` of a big-endian byte-aligned bitstream.
fn fetch_bit(bits: &[u8], index: usize) -> bool {
    bits[index / 8] >> (7 - index % 8) & 1 == 1
}

/// `symbol_bits`-bit symbol `index` of a big-endian byte-aligned bitstream.
fn fetch_symbol(bits: &[u8], index: usize, symbol_bits: usize) -> u8 {
    debug_assert_eq!(symbol_bits, 2);
    let offset = index * symbol_bits;
    (bits[offset / 8] >> (8 - offset % 8 - symbol_bits)) & 3
}

pub fn modulate_at(image: &mut [u8], geometry: Geometry, dx: usize, dy: usize, bits: &[u8]) {
    let width = geometry.width;
    for value in &mut image[..width * geometry.height] {
        *value = (*value).clamp(BASE_LOW, BASE_HIGH);
    }
    for row in 0..geometry.rows {
        modulate_row_at(image, geometry, dx, row * geometry.block + dy, bits, row);
    }
}

fn block_sum(image: &[u8], block: usize, width: usize, x0: usize, y0: usize) -> u64 {
    let mut sum = 0u64;
    for y in y0..y0 + block {
        sum += image[y * width + x0..y * width + x0 + block]
            .iter()
            .map(|&value| u64::from(value))
            .sum::<u64>();
    }
    sum
}

fn shift_towards(image: &mut [u8], block: usize, width: usize, x0: usize, y0: usize, delta: i32) {
    for y in y0..y0 + block {
        let start = y * width + x0;
        for value in &mut image[start..start + block] {
            *value = (i32::from(*value) + delta).clamp(30, 225) as u8;
        }
    }
}

pub fn modulate(image: &mut [u8], geometry: Geometry, bits: &[u8]) {
    let luma = geometry.width * geometry.height;
    let workers = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    // Every grid row owns a disjoint pixel band, so the rows can spread
    // over the worker pool. Tight machines stay serial: the FFmpeg encoder
    // and reader processes need those cores too, and sharing them measured
    // slower than the serial loop.
    if geometry.rows <= 2 || luma < 1 << 18 || workers < 6 {
        modulate_at(image, geometry, 0, 0, bits);
    } else {
        modulate_parallel(image, geometry, bits);
    }
}

/// Spreads the grid rows over the worker pool, four grid rows per task so a
/// 720p frame offers ~30 stealable chunks. Rows never share pixels (the
/// grid tiles the luma plane exactly), so this is a pure data split.
fn modulate_parallel(image: &mut [u8], geometry: Geometry, bits: &[u8]) {
    const ROWS_PER_TASK: usize = 4;
    let luma = geometry.width * geometry.height;
    let row_bytes = geometry.width * geometry.block;
    image[..luma]
        .par_chunks_mut(row_bytes * ROWS_PER_TASK)
        .enumerate()
        .for_each(|(chunk, slice)| {
            for value in slice.iter_mut() {
                *value = (*value).clamp(BASE_LOW, BASE_HIGH);
            }
            for inside in 0..ROWS_PER_TASK {
                let row = chunk * ROWS_PER_TASK + inside;
                if row >= geometry.rows {
                    break;
                }
                let offset = inside * row_bytes;
                modulate_row_at(
                    &mut slice[offset..offset + row_bytes],
                    geometry,
                    0,
                    0,
                    bits,
                    row,
                );
            }
        });
}

/// Applies one grid row's anchor blocks and pair modulation. `y0` is the
/// row's top edge: `dy`-shifted in the offset-aware serial path, band-local
/// zero in the parallel path where every grid row owns a disjoint pixel
/// band.
fn modulate_row_at(
    image: &mut [u8],
    geometry: Geometry,
    dx: usize,
    y0: usize,
    bits: &[u8],
    row: usize,
) {
    let width = geometry.width;
    let block = geometry.block;
    for col in 0..geometry.columns {
        if geometry.is_anchor(col, row) {
            fill_block(
                image,
                block,
                width,
                col * block + dx,
                y0,
                anchor_value(col, row),
            );
        }
    }
    let area = (block * block) as i64;
    let step = area * i64::from(MODULATION);
    let level = level_step(area);
    let symbols = bits.len() * 8 / geometry.symbol_bits;
    for pair in 0..geometry.pairs {
        let index = row * geometry.pairs + pair;
        if index >= symbols {
            continue;
        }
        let col = ANCHOR_COLS + 2 * pair;
        let x0 = col * block + dx;
        let x1 = (col + 1) * block + dx;
        let sum_a = i64::try_from(block_sum(image, block, width, x0, y0)).unwrap_or(0);
        let sum_b = i64::try_from(block_sum(image, block, width, x1, y0)).unwrap_or(0);
        let middle = (sum_a + sum_b) / 2;
        let (target_a, target_b) = if geometry.symbol_bits == 1 {
            if fetch_bit(bits, index) {
                (middle + step, middle - step)
            } else {
                (middle - step, middle + step)
            }
        } else {
            let symbol = fetch_symbol(bits, index, geometry.symbol_bits);
            let delta = (2 * symbol as i64 - 3) * level / 2;
            (middle + delta, middle - delta)
        };
        shift_towards(
            image,
            block,
            width,
            x0,
            y0,
            i32::try_from((target_a - sum_a) / area).unwrap_or(0),
        );
        shift_towards(
            image,
            block,
            width,
            x1,
            y0,
            i32::try_from((target_b - sum_b) / area).unwrap_or(0),
        );
    }
}

/// A reusable running-sum (integral image) scratch buffer that averages any
/// block of the frame in constant time.
#[derive(Default)]
pub struct Raster {
    integral: Vec<u64>,
    stride: usize,
    width: usize,
    height: usize,
}

impl Raster {
    pub fn scan(&mut self, y: &[u8], width: usize, height: usize) {
        self.integral.clear();
        self.integral.resize((width + 1) * (height + 1), 0);
        self.stride = width + 1;
        self.width = width;
        self.height = height;
        for row in 0..height {
            let mut sum = 0u64;
            for col in 0..width {
                sum += u64::from(y[row * width + col]);
                self.integral[(row + 1) * self.stride + (col + 1)] =
                    self.integral[row * self.stride + (col + 1)] + sum;
            }
        }
    }

    // Shifted candidate blocks can poke past the frame border, so the sums
    // clamp to the available pixels; partial edge blocks simply score lower.
    fn sum(&self, x0: usize, y0: usize, size: usize) -> u64 {
        let stride = self.stride;
        let x1 = (x0 + size).min(self.width);
        let y1 = (y0 + size).min(self.height);
        if x1 <= x0 || y1 <= y0 {
            return DIFFERENT_SUM_SENTINEL;
        }
        self.integral[y1 * stride + x1]
            - self.integral[y0 * stride + x1]
            - self.integral[y1 * stride + x0]
            + self.integral[y0 * stride + x0]
    }
}

/// Scores every grid shift by comparing each anchor block's mean luma with
/// the expected checkerboard sign; returns the best shift when it clearly
/// beats every other candidate.
pub fn find_alignment(y: &[u8], geometry: Geometry, raster: &mut Raster) -> Option<(usize, usize)> {
    raster.scan(y, geometry.width, geometry.height);
    let block = geometry.block;
    let mut best: Option<(usize, usize, i64)> = None;
    let mut runner_up = i64::MIN;
    for dy in 0..block {
        for dx in 0..block {
            let mut score = 0i64;
            for row in 0..geometry.rows {
                for col in 0..geometry.columns {
                    if !geometry.is_anchor(col, row) {
                        continue;
                    }
                    let mean = i64::try_from(raster.sum(col * block + dx, row * block + dy, block))
                        .unwrap_or(i64::MAX);
                    score += if (col + row) % 2 == 0 { mean } else { -mean };
                }
            }
            if score > best.map_or(i64::MIN, |(_, _, score)| score) {
                if let Some((_, _, previous)) = best {
                    runner_up = runner_up.max(previous);
                }
                best = Some((dx, dy, score));
            } else if score > runner_up {
                runner_up = score;
            }
        }
    }
    let (dx, dy, score) = best?;
    let margin = i64::from(ANCHOR_MARGIN) * geometry.anchor_count() as i64;
    (score - runner_up > margin).then_some((dx, dy))
}

/// Demodulates `out.len()` bytes from a frame. Legacy layers carry one bit
/// per pair (one when the first block's mean luma tops the second's);
/// current layers quantise the block-mean difference into four levels that
/// each stand for two bits. `raster.scan` must have run on the same `y`
/// plane first.
pub fn demodulate(geometry: Geometry, offset: (usize, usize), raster: &Raster, out: &mut [u8]) {
    let (dx, dy) = offset;
    let block = geometry.block;
    let area = (block * block) as i64;
    let level = level_step(area);
    let carrier = geometry.pairs * geometry.symbol_bits;
    for (byte_index, byte) in out.iter_mut().enumerate() {
        let mut value = 0u8;
        for bit in 0..8 {
            let index = byte_index * 8 + bit;
            let row = index / carrier;
            let within = index % carrier;
            let pair = within / geometry.symbol_bits;
            let sub = within % geometry.symbol_bits;
            let col = ANCHOR_COLS + 2 * pair;
            let a = raster.sum(col * block + dx, row * block + dy, block);
            let b = raster.sum((col + 1) * block + dx, row * block + dy, block);
            let one = if geometry.symbol_bits == 1 {
                a > b
            } else {
                // Nearest-level decision: the levels sit 2*level apart, so
                // (diff + 4*level) / (2*level) rounds to the level index
                // clamped to the four valid symbols.
                let diff = a as i64 - b as i64;
                let symbol = (diff + 4 * level).div_euclid(2 * level).clamp(0, 3);
                (symbol as u8 >> (1 - sub)) & 1 == 1
            };
            if one {
                value |= 1 << (7 - bit);
            }
        }
        *byte = value;
    }
}

/// Sizes a fresh frame reservoir before it is filled from FFmpeg.
const fn frame_len(geometry: Geometry) -> usize {
    geometry.width * geometry.height * 3 / 2
}

/// Spawns the two FFmpeg processes (the looping background reader and the
/// encoder that muxes the looping background audio), modulates the
/// Reed-Solomon stream into the frames, and returns the finished temporary
/// video.
pub(crate) fn encode<R: Read>(
    tools: &Tools,
    background: &Path,
    settings: &StealthSettings,
    layout: Layout,
    dir: &Path,
    mut stream: R,
) -> Result<NamedTempFile> {
    let geometry = Geometry::new(settings.width, settings.height, settings.block)?;
    let transport_header = transport(
        Grid {
            width: settings.width,
            height: settings.height,
            block: settings.block,
        },
        settings.parity,
        layout.protected,
        4,
        true,
        false,
    );
    let header_bytes = (MARKER.len() + TRANSPORT_SIZE) as u64;
    let coded = layout.protected - ECC_SIZE as u64;
    let frames = (header_bytes + coded).div_ceil(geometry.frame_bytes() as u64);
    let tmp = tempfile::Builder::new()
        .prefix(".file2video-")
        .tempfile_in(dir)
        .with_context(|| format!("could not create a temporary file in {}", dir.display()))?;
    let crf = settings.crf.to_string();
    let fps = settings.fps.to_string();
    let size = format!("{}x{}", settings.width, settings.height);

    let mut read_cmd = Command::new(&tools.ffmpeg);
    read_cmd
        .args(["-hide_banner", "-loglevel", "error", "-nostdin"])
        .args(["-stream_loop", "-1", "-i"])
        .arg(local_path(background))
        .args([
            "-vf",
            &if settings.rescale {
                format!(
                    "scale={}:{}:flags=bicubic,setsar=1",
                    settings.width, settings.height
                )
            } else {
                "setsar=1".to_owned()
            },
            "-r",
            &fps,
        ])
        .args(tools.frame_sync_cfr_args()?)
        .args(["-f", "rawvideo", "-pix_fmt", "yuv420p", "pipe:1"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped());
    let mut bg_process = tools.spawn(read_cmd)?;
    let bg_frames = BgFrames::start(
        bg_process
            .child
            .stdout
            .take()
            .context("missing FFmpeg stdout")?,
        frame_len(geometry),
    );

    let encoder = tools.select_encoder(settings.crf)?;
    let mut cmd = Command::new(&tools.ffmpeg);
    cmd.args(["-hide_banner", "-loglevel", "error", "-nostdin", "-y"])
        .args([
            "-f",
            "rawvideo",
            "-pixel_format",
            "yuv420p",
            "-video_size",
            &size,
            "-framerate",
            &fps,
            "-i",
            "pipe:0",
            "-stream_loop",
            "-1",
            "-i",
        ])
        .arg(local_path(background))
        .args(["-map", "0:v", "-map", "1:a:0?", "-shortest"]);
    if let Encoder::Gpu(backend) = encoder {
        for arg in backend.device_args() {
            cmd.arg(arg);
        }
    }
    if settings.intra {
        cmd.args(["-g", "1"]);
    }
    cmd.arg("-filter_threads").arg("1");
    let quality;
    match encoder {
        Encoder::Cpu => {
            cmd.args(["-vf", "format=yuv420p"])
                .args(["-c:v", "libx264", "-preset", &settings.preset])
                .args(["-crf", &crf])
                .args(["-threads:v", &tools.threads.to_string()])
                .args(["-pix_fmt", "yuv420p"]);
        }
        Encoder::Gpu(backend) => {
            let chain = backend.filter_chain("format=yuv420p");
            cmd.args(["-vf", &chain])
                .args(["-c:v", backend.encoder_name()]);
            quality = backend.quality_args(settings.crf, &settings.preset);
            cmd.args(&quality);
            if backend.passes_pix_fmt() {
                cmd.args(["-pix_fmt", "yuv420p"]);
            }
        }
    }
    cmd.args(["-c:a", "aac", "-b:a", "192k", "-f", "mp4"])
        .arg(local_path(tmp.path()))
        .stdin(Stdio::piped())
        .stdout(Stdio::null());
    let mut process = tools.spawn(cmd)?;
    let pipe = process.child.stdin.take().context("missing FFmpeg stdin")?;
    let mut writer = Writer {
        pipe,
        bg_frames,
        geometry,
        image: vec![0; frame_len(geometry)],
        pending: Vec::with_capacity(frame_len(geometry)),
        progress: Progress::new("Encode", frames),
        sent: 0,
        bytes: 0,
    };
    let outcome = (|| -> Result<u64> {
        let codec = ecc::Codec::new(settings.parity);
        writer.push(&MARKER)?;
        writer.push(&transport_header)?;
        let header = layout.ecc_header(settings.parity);
        codec.encode(
            header.as_slice().chain(stream.by_ref()),
            ECC_SIZE as u64 + layout.encrypted,
            |data| writer.push(data),
        )?;
        writer.finish()
    })();
    let sent = match outcome {
        Ok(sent) => sent,
        Err(error) => {
            drop(bg_process);
            return Err(process.explain(error));
        }
    };
    drop(bg_process);
    process.finish()?;
    ensure!(sent >= 1, "no stealth frames were written");
    Ok(tmp)
}

/// Prefetches background frames on a dedicated worker thread so the raw
/// FFmpeg pipe keeps being drained while the current frame is modulated and
/// encoded. The worker owns the child's stdout; dropping the receiver (and
/// then the pipe with it) makes FFmpeg exit with a broken pipe.
struct BgFrames {
    frames: Option<std::sync::mpsc::Receiver<Vec<u8>>>,
    worker: Option<std::thread::JoinHandle<()>>,
    len: usize,
}

impl BgFrames {
    fn start(pipe: ChildStdout, frame_len: usize) -> Self {
        let (sender, receiver) = std::sync::mpsc::sync_channel(2);
        let worker = std::thread::spawn(move || {
            let mut pipe = pipe;
            let mut frame = vec![0; frame_len];
            loop {
                if pipe.read_exact(&mut frame).is_err() || sender.send(frame.clone()).is_err() {
                    break;
                }
            }
        });
        Self {
            frames: Some(receiver),
            worker: Some(worker),
            len: frame_len,
        }
    }

    /// The next background frame, or an error once the background reader
    /// stopped (end of stream or FFmpeg failure).
    fn load(&mut self, image: &mut [u8]) -> Result<()> {
        let frame = self
            .frames
            .as_ref()
            .and_then(|frames| frames.recv().ok())
            .ok_or_else(|| anyhow::anyhow!("the stealth background video ended"))?;
        ensure!(frame.len() == self.len, "unexpected background frame size");
        image.copy_from_slice(&frame);
        Ok(())
    }
}

impl Drop for BgFrames {
    fn drop(&mut self) {
        // Dropping the receiver first unblocks a worker waiting on `send`;
        // joining afterwards can only wait for a pending pipes read at worst.
        if let Some(worker) = self.worker.take() {
            drop(self.frames.take());
            let _ = worker.join();
        }
    }
}

struct Writer {
    pipe: ChildStdin,
    bg_frames: BgFrames,
    geometry: Geometry,
    image: Vec<u8>,
    /// Bytes waiting for a free frame; every full `frame_bytes` chunk
    /// becomes one frame. Frame bit counts are multiples of eight, so frame
    /// boundaries sit on byte boundaries and no bit-level bookkeeping is
    /// needed while the payload arrives in byte chunks.
    pending: Vec<u8>,
    progress: Progress,
    sent: u64,
    bytes: u64,
}

impl Writer {
    fn push(&mut self, data: &[u8]) -> Result<()> {
        self.pending.extend_from_slice(data);
        let frame_bytes = self.geometry.frame_bytes();
        while self.pending.len() >= frame_bytes {
            self.emit_frame()?;
        }
        Ok(())
    }

    /// Modulates the next frame's worth of pending bytes onto the
    /// background image and writes it to the encoder.
    fn emit_frame(&mut self) -> Result<()> {
        self.bg_frames.load(&mut self.image)?;
        // The very last frame may carry fewer bits: missing bits keep the
        // clamped background, and rogue decoder output is caught by the
        // strict stream length plus Reed-Solomon parity.
        let frame_bytes = self.geometry.frame_bytes();
        let used = frame_bytes.min(self.pending.len());
        {
            let bits = &self.pending[..used];
            let image = self.image.as_mut_slice();
            let geometry = self.geometry;
            modulate(image, geometry, bits);
        }
        self.pipe
            .write_all(&self.image)
            .context("could not send a modulated frame to FFmpeg")?;
        self.pending.drain(..used);
        self.bytes += used as u64;
        self.sent += 1;
        self.progress.update(self.sent, self.bytes);
        Ok(())
    }

    /// Consumes the writer so the encoder's stdin is closed and FFmpeg
    /// finalises the file, and returns the number of frames written.
    fn finish(mut self) -> Result<u64> {
        if !self.pending.is_empty() {
            self.emit_frame()?;
        }
        Ok(self.sent)
    }
}

/// Blind stealth decoder: reads raw yuv420p frames from FFmpeg, finds the
/// anchor alignment on the first frame, verifies the stealth marker and
/// transport header, and exposes the remaining Reed-Solomon stream bytes
/// through `Read`.
pub(crate) struct Reader<R> {
    pipe: R,
    image: Vec<u8>,
    raster: Raster,
    geometry: Geometry,
    offset: (usize, usize),
    buffer: Vec<u8>,
    pos: usize,
    parity: usize,
    protected: u64,
    read: u64,
    bytes: u64,
    progress: Progress,
}

/// Even block sizes that could fit `width` x `height`, in probe order.
fn candidate_blocks(width: usize, height: usize) -> impl Iterator<Item = usize> {
    CANDIDATE_BLOCKS
        .into_iter()
        .filter(move |&block| width.is_multiple_of(block) && height.is_multiple_of(block))
}

impl<R: Read> Reader<R> {
    /// Opens a stealth stream without knowing the encoder's block size or
    /// modulation depth: every even candidate block that fits the resolution
    /// is demodulated with the quaternary (two bits per pair) and the legacy
    /// (one bit per pair) layers from the incoming frames until the sync
    /// marker, transport header and the header's own block size all agree on
    /// one candidate. The default block is probed first, then the faster
    /// smaller ones, then the sturdier larger ones. Only the frames needed
    /// for detection are consumed.
    pub(crate) fn open(mut pipe: R, width: usize, height: usize) -> Result<Self> {
        let luma = width * height;
        let mut image = vec![0; luma * 3 / 2];
        pipe.read_exact(&mut image)
            .map_err(|error| anyhow::anyhow!("no complete frame from FFmpeg: {error}"))?;
        let mut raster = Raster::default();
        raster.scan(&image[..luma], width, height);
        // Candidates that passed the sync-marker check, in probe order.
        let mut alive: Vec<(Geometry, (usize, usize), Vec<u8>)> = Vec::new();
        for block in candidate_blocks(width, height) {
            let Ok(probe) = Geometry::new(width, height, block) else {
                continue;
            };
            // The anchor checkerboard pins the alignment for every layer.
            let Some(at) = find_alignment(&image[..luma], probe, &mut raster) else {
                continue;
            };
            let mut variants = vec![probe];
            if let Ok(geometry) = Geometry::legacy(width, height, block) {
                variants.push(geometry);
            }
            for geometry in variants {
                let mut buffer = Vec::new();
                demodulate(
                    geometry,
                    at,
                    &raster,
                    push_frame(&mut buffer, geometry.frame_bytes()),
                );
                if buffer[..MARKER.len()] != MARKER {
                    continue;
                }
                alive.push((geometry, at, buffer));
            }
        }
        // Feed every surviving candidate until one transport header both
        // parses and matches the probed geometry.
        let mut winner: Option<(usize, Transport)> = None;
        let mut read = 1u64;
        loop {
            for (index, (geometry, _, buffer)) in alive.iter().enumerate() {
                if buffer.len() < HEADER_BYTES {
                    continue;
                }
                let Ok(parsed) = parse_transport(&buffer[MARKER.len()..HEADER_BYTES]) else {
                    continue;
                };
                if parsed.width == width
                    && parsed.height == height
                    && parsed.block == geometry.block
                {
                    winner = Some((index, parsed));
                    break;
                }
            }
            if winner.is_some() {
                break;
            }
            // Headers that parsed damaged eliminate their candidate.
            alive.retain(|(_, _, buffer)| buffer.len() < HEADER_BYTES);
            ensure!(
                !alive.is_empty(),
                "not a stealth file2video video; the sync markers or transport header do not match"
            );
            pipe.read_exact(&mut image)
                .map_err(|error| anyhow::anyhow!("no complete frame from FFmpeg: {error}"))?;
            raster.scan(&image[..luma], width, height);
            for (geometry, at, buffer) in &mut alive {
                demodulate(
                    *geometry,
                    *at,
                    &raster,
                    push_frame(buffer, geometry.frame_bytes()),
                );
            }
            read += 1;
        }
        let (index, parsed) = winner.expect("the loop ends when a winner is found");
        let (geometry, offset, buffer) = alive.swap_remove(index);
        let stream_len = HEADER_BYTES as u64 + parsed.protected - ECC_SIZE as u64;
        Ok(Self {
            pipe,
            image,
            raster,
            geometry,
            offset,
            buffer,
            pos: HEADER_BYTES,
            parity: parsed.parity,
            protected: parsed.protected,
            read,
            bytes: 0,
            progress: Progress::new("Decode", stream_len.div_ceil(geometry.frame_bytes() as u64)),
        })
    }

    pub(crate) fn parity(&self) -> usize {
        self.parity
    }

    pub(crate) fn protected(&self) -> u64 {
        self.protected
    }

    fn next_frame(&mut self) -> std::io::Result<()> {
        let frame_bytes = self.geometry.frame_bytes();
        self.buffer.resize(frame_bytes, 0);
        self.pipe.read_exact(&mut self.image)?;
        self.raster.scan(
            &self.image[..self.geometry.width * self.geometry.height],
            self.geometry.width,
            self.geometry.height,
        );
        demodulate(
            self.geometry,
            self.offset,
            &self.raster,
            &mut self.buffer[..frame_bytes],
        );
        self.pos = 0;
        self.read += 1;
        self.bytes += frame_bytes as u64;
        self.progress.update(self.read, self.bytes);
        Ok(())
    }

    pub(crate) fn finish(mut self) -> Result<()> {
        let mut extra = [0; 1];
        ensure!(
            self.pipe.read(&mut extra)? == 0,
            "unexpected extra video frames"
        );
        Ok(())
    }
}

/// Grows `buffer` by one frame and returns the fresh tail for demodulation.
fn push_frame(buffer: &mut Vec<u8>, frame_bytes: usize) -> &mut [u8] {
    let start = buffer.len();
    buffer.resize(start + frame_bytes, 0);
    &mut buffer[start..]
}

impl<R: Read> Read for Reader<R> {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        if self.pos == self.buffer.len() {
            self.next_frame()?;
        }
        let n = out.len().min(self.buffer.len() - self.pos);
        out[..n].copy_from_slice(&self.buffer[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Geometry, Raster, demodulate, find_alignment, modulate, modulate_at, modulate_parallel,
    };

    const WIDTH: usize = 240;
    const HEIGHT: usize = 144;

    fn background_for(width: usize, height: usize) -> Vec<u8> {
        (0..width * height * 3 / 2)
            .map(|i| (30 + (i * 37 + i / width * 11) % 221) as u8)
            .collect()
    }

    fn background() -> Vec<u8> {
        background_for(WIDTH, HEIGHT)
    }

    #[test]
    fn geometry_matches_the_documented_capacities() {
        let full = Geometry::new(1920, 1080, super::BLOCK).unwrap();
        assert_eq!(full.frame_bytes(), 15893);
        let small = Geometry::new(WIDTH, HEIGHT, super::BLOCK).unwrap();
        assert_eq!(small.frame_bytes(), 232);
        // Block 6 keeps the older sturdier cell size (0.44x the payload).
        assert_eq!(Geometry::new(WIDTH, HEIGHT, 6).unwrap().frame_bytes(), 95);
        // Block 2 packs (4/2)^2 = 4x as many bit pairs into the same frame;
        // integer rounding gives 2.4x at this resolution.
        let dense = Geometry::new(WIDTH, HEIGHT, 2).unwrap();
        assert_eq!(dense.frame_bytes(), 1003);
        assert!(
            dense.frame_bytes() * 4 > small.frame_bytes() * 9,
            "block 2 must carry at least the documented 4x density"
        );
        // The legacy layer carries exactly half the bits per frame.
        assert_eq!(
            Geometry::legacy(WIDTH, HEIGHT, super::BLOCK)
                .unwrap()
                .frame_bytes(),
            small.frame_bytes() / 2
        );
        assert!(Geometry::new(24, 12, super::BLOCK).is_err());
    }

    #[test]
    fn modulation_round_trips_through_clamping() {
        // Both the default and the sturdier block must round trip.
        for block in [super::BLOCK, 6] {
            round_trip(block);
        }
    }

    fn round_trip(block: usize) {
        let geometry = Geometry::new(WIDTH, HEIGHT, block).unwrap();
        round_trip_with(geometry);
    }

    #[test]
    fn legacy_layer_still_round_trips_one_bit_per_pair() {
        // Videos encoded before the density increase still restore: the
        // decoder probes the one-bit-per-pair layer beside the current one.
        for block in [super::BLOCK, 6] {
            round_trip_with(Geometry::legacy(WIDTH, HEIGHT, block).unwrap());
        }
    }

    fn round_trip_with(geometry: Geometry) {
        let bits: Vec<u8> = (0..geometry.frame_bytes())
            .map(|byte| (byte % 7) as u8)
            .collect();
        let mut image = background();
        modulate(&mut image, geometry, &bits);
        let mut raster = Raster::default();
        let alignment = find_alignment(&image[..WIDTH * HEIGHT], geometry, &mut raster);
        assert_eq!(alignment, Some((0, 0)));
        raster.scan(&image[..WIDTH * HEIGHT], WIDTH, HEIGHT);
        let mut out = vec![0; geometry.frame_bytes()];
        demodulate(geometry, alignment.unwrap(), &raster, &mut out);
        for index in 0..geometry.frame_bytes() * 8 {
            let bit = bits[index / 8] >> (7 - index % 8) & 1 == 1;
            assert_eq!(
                out[index / 8] >> (7 - index % 8) & 1 == 1,
                bit,
                "bit {index} (pair {}, digit {}) of block {}",
                index / geometry.symbol_bits,
                index % geometry.symbol_bits,
                geometry.block
            );
        }
        for &value in &image[..WIDTH * HEIGHT] {
            assert!(
                (30..=225).contains(&value),
                "luma {value} escaped the safe range"
            );
        }
    }

    #[test]
    fn alignment_rejects_a_plain_background() {
        let geometry = Geometry::new(WIDTH, HEIGHT, super::BLOCK).unwrap();
        let mut raster = Raster::default();
        assert_eq!(
            find_alignment(&background()[..WIDTH * HEIGHT], geometry, &mut raster),
            None
        );
    }

    #[test]
    fn parallel_modulation_matches_the_serial_loop() {
        // Forced on machines with few cores: the CI runners never reach the
        // available_parallelism() >= 6 gate, so exercise the pool split
        // directly and require bitwise-identical output.
        let geometry = Geometry::new(WIDTH * 8, HEIGHT * 8, 4).unwrap();
        let bits: Vec<u8> = (0..geometry.frame_bytes())
            .map(|byte| (byte % 13) as u8)
            .collect();
        let mut serial = background_for(geometry.width, geometry.height);
        modulate_at(&mut serial, geometry, 0, 0, &bits);
        let mut parallel = background_for(geometry.width, geometry.height);
        modulate_parallel(&mut parallel, geometry, &bits);
        assert_eq!(serial, parallel);
    }
}
