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
    ECC_SIZE, Grid, Layout, TRANSPORT_SIZE, Transport, parse_transport, stealth_transport,
    validate_resolution,
};
use crate::video::{Encoder, Progress, Tools, local_path};
use anyhow::{Context, Result, ensure};
use rayon::prelude::*;
use std::io::{Read, Write};
use std::path::Path;
use std::process::{ChildStdin, ChildStdout, Command, Stdio};
use tempfile::NamedTempFile;

/// Pixels per modulation block; every data symbol occupies a pair of two
/// horizontally adjacent blocks. This is the default; `--stealth-block`
/// overrides it (smaller blocks carry more data per frame). Block 4 gets
/// a 1 GiB archive close to a regular encode's wall time; block 6 stays
/// available for maximum robustness through very aggressive re-encoding.
pub const BLOCK: usize = 4;
/// Even block sizes tried by the blind decoder, in probe order: the default
/// first, then the faster smaller blocks, then the sturdier larger ones.
const CANDIDATE_BLOCKS: [usize; 8] = [4, 6, 2, 8, 10, 12, 14, 16];
/// The data layer's outer luma level: the modulation levels sit at ±`intensity`
/// and, at two bits per pair, ±`intensity`/3. Lower values hide the layer
/// better; higher values are more visible but survive heavy re-encoding
/// (YouTube, streaming platforms) far better. `--stealth-intensity`/
/// `stealth_intensity` override the default.
pub const INTENSITY: i16 = 25;
/// The valid `--stealth-intensity` range. At the maximum the background
/// still keeps 50 luma levels of its own range, and every modulation level
/// stays reachable within the 30..=225 sample clamp at any block brightness.
pub const MIN_INTENSITY: i16 = 5;
pub const MAX_INTENSITY: i16 = 50;
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

/// Fixed stealth-layer geometry derived from the output resolution, the
/// modulation block size, the modulation depth and the layer intensity.
#[derive(Clone, Copy, Debug)]
pub struct Geometry {
    pub width: usize,
    pub height: usize,
    pub block: usize,
    /// Bits carried by one block pair: 2 for the default quaternary
    /// modulation, 4 for the dense sixteen-level layer, and 1 for the legacy
    /// binary layer (decode compatibility).
    pub symbol_bits: usize,
    /// The layer's outer luma level (see [`INTENSITY`]).
    pub intensity: i16,
    columns: usize,
    rows: usize,
    pairs: usize,
    bits: usize,
}

impl Geometry {
    /// The current stealth layout: two modulation bits per block pair.
    pub fn new(width: usize, height: usize, block: usize) -> Result<Self> {
        Self::build(width, height, block, 2, INTENSITY)
    }

    /// The dense stealth layout: four modulation bits per block pair across
    /// sixteen levels with the same outer span, twice the payload per frame.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn dense(width: usize, height: usize, block: usize) -> Result<Self> {
        Self::build(width, height, block, 4, INTENSITY)
    }

    /// The pre-density-stealth layout: one modulation bit per block pair.
    /// Encoding no longer produces this; the decoder still probes for it so
    /// older videos keep restoring.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn legacy(width: usize, height: usize, block: usize) -> Result<Self> {
        Self::build(width, height, block, 1, INTENSITY)
    }

    /// The full-parameter constructor: `symbol_bits` selects the modulation
    /// depth (1 legacy, 2 default, 4 dense) and `intensity` the outer luma
    /// level of the data layer.
    pub fn build(
        width: usize,
        height: usize,
        block: usize,
        symbol_bits: usize,
        intensity: i16,
    ) -> Result<Self> {
        validate_resolution(width, height)?;
        ensure!(
            width.is_multiple_of(block) && height.is_multiple_of(block),
            "the stealth video resolution must be a multiple of {block} pixels"
        );
        ensure!(
            matches!(symbol_bits, 1 | 2 | 4),
            "unsupported modulation depth"
        );
        ensure!(
            (MIN_INTENSITY..=MAX_INTENSITY).contains(&intensity),
            "the stealth intensity must be in {MIN_INTENSITY}..={MAX_INTENSITY}"
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
            intensity,
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

    /// The data rows also cover the top rows' inner blocks (the encode and
    /// decode agree on that layout), so only the border side columns and the
    /// bottom anchor row's blocks actually carry the checkerboard. The
    /// alignment score and the intensity measurement must not count the
    /// overwritten positions: their data content would dilute both.
    fn carries_anchor(&self, col: usize, row: usize) -> bool {
        self.is_anchor(col, row)
            && (col < ANCHOR_COLS
                || col >= self.columns - ANCHOR_COLS
                || row >= self.rows - ANCHOR_TOP_ROWS - ANCHOR_BOTTOM_ROWS)
    }

    fn anchor_count(&self) -> usize {
        (0..self.rows)
            .map(|row| {
                (0..self.columns)
                    .filter(|&col| self.carries_anchor(col, row))
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
    /// Modulation bits per block pair: 2 by default, 4 with `--stealth-dense`
    /// (double the data rate over the same outer luma span).
    pub symbol_bits: usize,
    /// The data layer's outer luma level (default `INTENSITY`); higher is
    /// more visible and survives aggressive re-encoding better.
    pub intensity: i16,
    pub fps: u8,
    pub crf: u8,
    pub preset: String,
    pub intra: bool,
    /// Reed-Solomon parity per 255-byte codeword. Stealth defaults to 96,
    /// roughly 38% redundancy, so heavy compression noise stays correctable.
    pub parity: usize,
}

fn anchor_value(col: usize, row: usize, intensity: i16) -> u8 {
    if (col + row).is_multiple_of(2) {
        ANCHOR_MID + intensity as u8
    } else {
        ANCHOR_MID - intensity as u8
    }
}

fn fill_block(image: &mut [u8], block: usize, width: usize, x0: usize, y0: usize, value: u8) {
    for y in y0..y0 + block {
        let start = y * width + x0;
        image[start..start + block].fill(value);
    }
}

/// Sum distance between adjacent modulation levels for a block of `area`:
/// `n` levels (with `n = 1 << symbol_bits`) span the same ±`intensity` luma
/// range, so the step narrows as the depth grows. Both the modulator and the
/// demodulator derive this from the same formula, so quantisation is exact.
fn level_step(area: i64, symbol_bits: usize, intensity: i16) -> i64 {
    area * 2 * i64::from(intensity) / ((1i64 << symbol_bits) - 1)
}

/// The background luma is clamped to a range that leaves `intensity` levels
/// of headroom to the fixed 30..=225 sample extremes, so every modulation
/// level stays reachable from any clamped block. At the default intensity
/// 25 this reproduces the original 55..=200 window.
fn base_range(intensity: i16) -> (u8, u8) {
    ((30 + intensity) as u8, (225 - intensity) as u8)
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
    let offset = index * symbol_bits;
    (bits[offset / 8] >> (8 - offset % 8 - symbol_bits)) & ((1 << symbol_bits) - 1)
}

pub fn modulate_at(image: &mut [u8], geometry: Geometry, dx: usize, dy: usize, bits: &[u8]) {
    let width = geometry.width;
    let (low, high) = base_range(geometry.intensity);
    for value in &mut image[..width * geometry.height] {
        *value = (*value).clamp(low, high);
    }
    for row in 0..geometry.rows {
        modulate_row_at(image, geometry, dx, row * geometry.block + dy, bits, row);
    }
}

/// Per-block scan results gathered in a single read pass: the pixel sum,
/// the total headroom towards the fixed 30..=225 sample extremes, and the
/// extremes themselves (with the run length of the maximum) that gate the
/// uniform-share fast path of `set_block_sum`.
#[derive(Clone, Copy)]
struct BlockStats {
    sum: i32,
    head_down: i32,
    head_up: i32,
    min: i32,
    max: i32,
    max_count: i32,
}

impl BlockStats {
    fn empty() -> Self {
        Self {
            sum: 0,
            head_down: 0,
            head_up: 0,
            min: i32::MAX,
            max: i32::MIN,
            max_count: 0,
        }
    }

    fn add(&mut self, value: u8) {
        let v = i32::from(value);
        self.sum += v;
        self.head_down += v - 30;
        self.head_up += 225 - v;
        if v < self.min {
            self.min = v;
        }
        if v > self.max {
            self.max = v;
            self.max_count = 1;
        } else if v == self.max {
            self.max_count += 1;
        }
    }
}

/// Rewrites one block so its pixel sum hits `target` exactly. A uniform
/// shift cannot always reach the target (individual pixels clamp against
/// the fixed 30..=225 extremes long before the pair-mean correction is
/// done), and the truncation loss of a uniform shift is up to `area` sum
/// units - more than the dense layer's quantisation half-step.
///
/// The common case shares the needed change evenly: every pixel takes the
/// same step and the first `remaining % area` pixels take one extra unit,
/// taken only when neither the shared step nor the run-up can clamp any
/// pixel (the scan's min/max and max run length prove that). The rare
/// clamping case falls back to pixel-by-pixel distribution with suffix
/// headroom bookkeeping: every pixel moves as far as needed while staying
/// inside 30..=225, and the pair's mean-centring and level shift land
/// exactly.
///
/// Feasibility is guaranteed: `base_range` clamps the background to
/// [30+intensity, 225-intensity] and the modulation levels never ask for
/// more than `intensity * area` of sum change, so the target always lies
/// within the block's reachable 30*area..=225*area sum span.
fn set_block_sum(
    image: &mut [u8],
    block: usize,
    width: usize,
    x0: usize,
    y0: usize,
    stats: BlockStats,
    target: i64,
) {
    let area = (block * block) as i32;
    let target = target.clamp(i64::from(30 * area), i64::from(225 * area)) as i32;
    let remaining = target - stats.sum;
    if remaining == 0 {
        return;
    }
    let share = remaining.div_euclid(area);
    let rest = remaining.rem_euclid(area);
    let share_fits = stats.min + share >= 30 && stats.max + share <= 225;
    let run_up_fits = rest == 0 || stats.max + share <= 224;
    if share_fits && run_up_fits {
        let mut extra = rest;
        for y in y0..y0 + block {
            let start = y * width + x0;
            for value in &mut image[start..start + block] {
                let mut v = i32::from(*value) + share;
                if extra > 0 && v <= 224 {
                    v += 1;
                    extra -= 1;
                }
                *value = v as u8;
            }
        }
        debug_assert_eq!(extra, 0, "the run-up must cover the whole remainder");
        return;
    }
    let mut remaining = remaining;
    // Spread the needed change as an even per-pixel share, then give each
    // pixel the share it can actually take: the share is clamped to the
    // interval that still leaves the not-yet-visited pixels enough headroom
    // to absorb what is left, and to this pixel's own 30..=225 room.
    let mut seen_down = 0i32;
    let mut seen_up = 0i32;
    for y in y0..y0 + block {
        let start = y * width + x0;
        for (inner, value) in image[start..start + block].iter_mut().enumerate() {
            let position = (y - y0) * block + inner;
            let (down, up) = (i32::from(*value) - 30, 225 - i32::from(*value));
            let suffix_down = stats.head_down - seen_down - down;
            let suffix_up = stats.head_up - seen_up - up;
            let give = (remaining / (area - position as i32))
                .clamp(remaining - suffix_up, remaining + suffix_down)
                .clamp(-down, up);
            *value = (*value as i32 + give) as u8;
            remaining -= give;
            seen_down += down;
            seen_up += up;
        }
    }
    debug_assert_eq!(remaining, 0, "the modulation target must be reachable");
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
    let (low, high) = base_range(geometry.intensity);
    image[..luma]
        .par_chunks_mut(row_bytes * ROWS_PER_TASK)
        .enumerate()
        .for_each(|(chunk, slice)| {
            for value in slice.iter_mut() {
                *value = (*value).clamp(low, high);
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
                anchor_value(col, row, geometry.intensity),
            );
        }
    }
    let area = (block * block) as i64;
    let step = if geometry.symbol_bits == 1 {
        area * i64::from(geometry.intensity)
    } else {
        level_step(area, geometry.symbol_bits, geometry.intensity)
    };
    let symbols = bits.len() * 8 / geometry.symbol_bits;
    for pair in 0..geometry.pairs {
        let index = row * geometry.pairs + pair;
        if index >= symbols {
            continue;
        }
        let col = ANCHOR_COLS + 2 * pair;
        let x0 = col * block + dx;
        // One scan over the pair's two adjacent blocks gathers both sums
        // plus every headroom bookkeeping the write pass needs, replacing
        // the older separate read passes per block.
        let mut stats_a = BlockStats::empty();
        let mut stats_b = BlockStats::empty();
        for y in y0..y0 + block {
            let row_pixels = &image[y * width + x0..y * width + x0 + 2 * block];
            for &value in &row_pixels[..block] {
                stats_a.add(value);
            }
            for &value in &row_pixels[block..] {
                stats_b.add(value);
            }
        }
        let x1 = (col + 1) * block + dx;
        let middle = i64::from((stats_a.sum + stats_b.sum) / 2);
        let (target_a, target_b) = if geometry.symbol_bits == 1 {
            if fetch_bit(bits, index) {
                (middle + step, middle - step)
            } else {
                (middle - step, middle + step)
            }
        } else {
            // `n` evenly spaced levels over ±`intensity` luma; the encoding
            // and decoding steps are identical so target rounding is exact.
            let symbol = fetch_symbol(bits, index, geometry.symbol_bits);
            let delta = (2 * symbol as i64 - ((1i64 << geometry.symbol_bits) - 1)) * step / 2;
            (middle + delta, middle - delta)
        };
        set_block_sum(image, block, width, x0, y0, stats_a, target_a);
        set_block_sum(image, block, width, x1, y0, stats_b, target_b);
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
                    if !geometry.carries_anchor(col, row) {
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
/// current layers quantise the block-mean difference into 4 or 16 levels
/// that each stand for `symbol_bits` bits. `raster.scan` must have run on
/// the same `y` plane first.
pub fn demodulate(geometry: Geometry, offset: (usize, usize), raster: &Raster, out: &mut [u8]) {
    let (dx, dy) = offset;
    let block = geometry.block;
    let area = (block * block) as i64;
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
                // Nearest-level decision: the levels sit 2*step apart, so
                // (diff + n*step) / (2*step) rounds to the level index,
                // clamped to the valid symbols.
                let levels = 1i64 << geometry.symbol_bits;
                let step = level_step(area, geometry.symbol_bits, geometry.intensity);
                let diff = a as i64 - b as i64;
                let symbol = (diff + levels * step)
                    .div_euclid(2 * step)
                    .clamp(0, levels - 1);
                (symbol as u8 >> (geometry.symbol_bits - 1 - sub)) & 1 == 1
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
    let geometry = Geometry::build(
        settings.width,
        settings.height,
        settings.block,
        settings.symbol_bits,
        settings.intensity,
    )?;
    let transport_header = stealth_transport(
        Grid {
            width: settings.width,
            height: settings.height,
            block: settings.block,
        },
        settings.parity,
        layout.protected,
        settings.symbol_bits as u8,
        settings.intensity as u8,
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
    let mut writer = Writer::start(pipe, bg_frames, geometry, Progress::new("Encode", frames));
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

/// A modulated frame waiting for the encoder pipe. `bits` counts the
/// payload bytes the frame carries (the final frame may carry fewer).
struct Frame {
    image: Vec<u8>,
    bits: usize,
}

/// Pipelined encoder feeder: a producer worker overlaps the modulation of
/// the next frame with the FFmpeg encoder consuming the current one, while
/// the writer thread only ships ready frames down the pipe. The producer's
/// image buffers recycle through the `spares` channel, so steady state
/// allocates nothing.
struct Writer {
    pipe: ChildStdin,
    /// Payload chunks for the producer; dropping it ends the stream.
    inbox: Option<std::sync::mpsc::Sender<Vec<u8>>>,
    /// Ready frames back from the producer, bounded to keep at most a
    /// couple of frames in flight.
    frames: Option<std::sync::mpsc::Receiver<Frame>>,
    /// Spent image buffers returned to the producer for reuse.
    spares: Option<std::sync::mpsc::Sender<Vec<u8>>>,
    failure: std::sync::Arc<std::sync::Mutex<Option<anyhow::Error>>>,
    worker: Option<std::thread::JoinHandle<()>>,
    sent: u64,
    bytes: u64,
}

impl Writer {
    fn start(
        pipe: ChildStdin,
        bg_frames: BgFrames,
        geometry: Geometry,
        progress: Progress,
    ) -> Self {
        let (inbox, inbox_rx) = std::sync::mpsc::channel();
        let (frame_tx, frames) = std::sync::mpsc::sync_channel(2);
        let (spares_tx, spares) = std::sync::mpsc::channel();
        let failure = std::sync::Arc::new(std::sync::Mutex::new(None));
        let worker_failure = std::sync::Arc::clone(&failure);
        let worker = std::thread::spawn(move || {
            if let Err(error) =
                write_frames(bg_frames, geometry, progress, inbox_rx, frame_tx, spares)
            {
                *worker_failure.lock().unwrap() = Some(error);
            }
        });
        Self {
            pipe,
            inbox: Some(inbox),
            frames: Some(frames),
            spares: Some(spares_tx),
            failure,
            worker: Some(worker),
            sent: 0,
            bytes: 0,
        }
    }

    fn check_failure(&self) -> Result<()> {
        match self.failure.lock().unwrap().take() {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    fn push(&mut self, data: &[u8]) -> Result<()> {
        self.check_failure()?;
        if !data.is_empty() {
            let inbox = self
                .inbox
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("the stealth frame worker already stopped"))?;
            if inbox.send(data.to_vec()).is_err() {
                self.check_failure()?;
                anyhow::bail!("the stealth frame worker stopped unexpectedly");
            }
        }
        // Ship every frame that is already ready so the producer never runs
        // further ahead than its bounded frame queue.
        loop {
            match self.frames.as_ref().expect("writer dropping").try_recv() {
                Ok(frame) => self.write_frame(frame)?,
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    self.check_failure()?;
                    anyhow::bail!("the stealth frame worker stopped unexpectedly");
                }
            }
        }
        Ok(())
    }

    fn write_frame(&mut self, frame: Frame) -> Result<()> {
        self.pipe
            .write_all(&frame.image)
            .context("could not send a modulated frame to FFmpeg")?;
        self.bytes += frame.bits as u64;
        self.sent += 1;
        if let Some(spares) = self.spares.as_ref() {
            let _ = spares.send(frame.image);
        }
        Ok(())
    }

    /// Consumes the writer: closes the payload inbox (which ends the
    /// producer's stream, final partial frame included), writes every
    /// remaining ready frame, closes the encoder's stdin so FFmpeg finalises
    /// the file, and returns the number of frames written.
    fn finish(mut self) -> Result<u64> {
        drop(self.inbox.take());
        drop(self.spares.take());
        let frames = self.frames.take();
        if let Some(frames) = frames.as_ref() {
            while let Ok(frame) = frames.recv() {
                self.write_frame(frame)?;
            }
        }
        drop(frames);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        self.check_failure()?;
        ensure!(self.sent >= 1, "no stealth frames were written");
        Ok(self.sent)
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        // Unblock the worker from both sides so a failed encode cannot
        // leave a thread waiting on a channel nobody reads anymore.
        drop(self.inbox.take());
        drop(self.frames.take());
        drop(self.spares.take());
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

/// The producer side of the writer pipeline: modulates payload bytes onto
/// background frames and hands the results to the writer thread.
fn write_frames(
    mut bg_frames: BgFrames,
    geometry: Geometry,
    mut progress: Progress,
    inbox: std::sync::mpsc::Receiver<Vec<u8>>,
    frames: std::sync::mpsc::SyncSender<Frame>,
    spares: std::sync::mpsc::Receiver<Vec<u8>>,
) -> Result<()> {
    let frame_bytes = geometry.frame_bytes();
    let image_len = frame_len(geometry);
    let mut pending: Vec<u8> = Vec::new();
    let mut sent = 0u64;
    let mut bytes = 0u64;
    let mut render = |pending: &[u8], used: usize| -> Result<Frame> {
        let mut image = spares.try_recv().unwrap_or_else(|_| vec![0; image_len]);
        bg_frames.load(&mut image)?;
        modulate(&mut image, geometry, &pending[..used]);
        Ok(Frame { image, bits: used })
    };
    let mut done = false;
    while !done {
        loop {
            match inbox.recv() {
                Ok(chunk) => {
                    pending.extend_from_slice(&chunk);
                    if pending.len() >= frame_bytes {
                        break;
                    }
                }
                Err(_) => {
                    done = true;
                }
            }
            if done {
                break;
            }
        }
        // Frame bit counts are multiples of eight, so frame boundaries sit
        // on byte boundaries and no bit-level bookkeeping is needed while
        // the payload arrives in byte chunks.
        while pending.len() >= frame_bytes {
            let frame = render(&pending, frame_bytes)?;
            pending.drain(..frame_bytes);
            if frames.send(frame).is_err() {
                anyhow::bail!("the stealth writer stopped accepting frames");
            }
            sent += 1;
            bytes += frame_bytes as u64;
            progress.update(sent, bytes);
        }
    }
    // The very last frame may carry fewer bits: missing bits keep the
    // clamped background, and rogue decoder output is caught by the strict
    // stream length plus Reed-Solomon parity.
    if !pending.is_empty() {
        let used = pending.len();
        let frame = render(&pending, used)?;
        let _ = frames.send(frame);
        sent += 1;
        bytes += used as u64;
        progress.update(sent, bytes);
    }
    Ok(())
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
    /// Opens a stealth stream without knowing the encoder's block size,
    /// modulation depth or intensity: every even candidate block that fits
    /// the resolution is demodulated with the quaternary (two bits per
    /// pair), dense (four bits per pair) and legacy (one bit per pair)
    /// layers from the incoming frames until the sync marker, transport
    /// header and the header's own block size all agree on one candidate.
    /// The layer intensity is measured from the anchor checkerboard, so the
    /// probe matches the written layer without trying every setting. The
    /// default block is probed first, then the faster smaller ones, then
    /// the sturdier larger ones. Only the frames needed for detection are
    /// consumed.
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
            let intensity = measured_intensity(&raster, probe, at);
            for symbol_bits in [2usize, 4, 1] {
                let Ok(geometry) = Geometry::build(width, height, block, symbol_bits, intensity)
                else {
                    continue;
                };
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
                // Videos written before the depth/intensity fields existed
                // leave both at zero; trust the probe for those.
                let depth_match =
                    parsed.symbol_bits == 0 || parsed.symbol_bits as usize == geometry.symbol_bits;
                let intensity_match = parsed.intensity as i64 == 0
                    || (i64::from(parsed.intensity) - i64::from(geometry.intensity)).abs() <= 2;
                if parsed.width == width
                    && parsed.height == height
                    && parsed.block == geometry.block
                    && depth_match
                    && intensity_match
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

/// Estimates the layer intensity from the anchor checkerboard: the even
/// anchor blocks sit at `ANCHOR_MID + intensity` luma, the odd ones at
/// `ANCHOR_MID - intensity`, and averaging hundreds of pixels swamps the
/// compression noise. `raster` must have scanned the same plane.
fn measured_intensity(raster: &Raster, geometry: Geometry, (dx, dy): (usize, usize)) -> i16 {
    let block = geometry.block;
    let area = block * block;
    let mut even = 0i64;
    let mut odd = 0i64;
    for row in 0..geometry.rows {
        for col in 0..geometry.columns {
            if !geometry.carries_anchor(col, row) {
                continue;
            }
            let sum =
                i64::try_from(raster.sum(col * block + dx, row * block + dy, block)).unwrap_or(0);
            if (col + row).is_multiple_of(2) {
                even += sum;
            } else {
                odd += sum;
            }
        }
    }
    let pixels = (geometry.anchor_count() * area) as i64;
    // Even and odd anchor blocks are equally numerous; their pixel sum
    // difference is `pixels * intensity`.
    let estimate =
        ((even - odd) / pixels).clamp(i64::from(MIN_INTENSITY), i64::from(MAX_INTENSITY));
    estimate as i16
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
        // Dense doubles the bits per frame (four per pair) over the same
        // block grid; truncation to whole bytes cannot lose more than one.
        let dense = Geometry::dense(WIDTH, HEIGHT, super::BLOCK).unwrap();
        assert_eq!(dense.frame_bytes(), 464);
        assert!(
            dense.frame_bytes() >= small.frame_bytes() * 2 - 1,
            "dense must double the payload per frame"
        );
        // The intensity only spreads the same levels wider or narrower.
        assert_eq!(
            Geometry::build(WIDTH, HEIGHT, super::BLOCK, 2, 50)
                .unwrap()
                .frame_bytes(),
            small.frame_bytes()
        );
        assert!(
            Geometry::build(WIDTH, HEIGHT, super::BLOCK, 2, 4).is_err(),
            "the intensity range must be bounded"
        );
        assert!(Geometry::new(24, 12, super::BLOCK).is_err());
    }

    #[test]
    fn modulation_round_trips_through_clamping() {
        // The default and sturdier blocks, the dense sixteen-level layer and
        // both intensity extremes must all round trip.
        for block in [super::BLOCK, 6] {
            round_trip(block);
            round_trip_with(Geometry::build(WIDTH, HEIGHT, block, 4, super::INTENSITY).unwrap());
            round_trip_with(Geometry::build(WIDTH, HEIGHT, block, 2, 50).unwrap());
            round_trip_with(Geometry::build(WIDTH, HEIGHT, block, 2, 5).unwrap());
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
