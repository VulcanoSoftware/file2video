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
use std::io::{Read, Write};
use std::path::Path;
use std::process::{ChildStdin, ChildStdout, Command, Stdio};
use std::sync::mpsc;
use tempfile::NamedTempFile;

/// Pixels per modulation block; every data bit occupies a pair of two
/// horizontally adjacent blocks. This is the default; `--stealth-block`
/// overrides it (smaller blocks carry more data per frame).
pub const BLOCK: usize = 6;
/// Even block sizes tried by the blind decoder, in probe order: the default
/// first, then the faster smaller blocks, then the sturdier larger ones.
const CANDIDATE_BLOCKS: [usize; 8] = [6, 4, 2, 8, 10, 12, 14, 16];
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
    columns: usize,
    rows: usize,
    pairs: usize,
    bits: usize,
}

impl Geometry {
    pub fn new(width: usize, height: usize, block: usize) -> Result<Self> {
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
        let bits = pairs * (rows - ANCHOR_TOP_ROWS - ANCHOR_BOTTOM_ROWS) / 8 * 8;
        ensure!(
            bits >= 8 * 32,
            "the video is too small to carry a stealth data layer"
        );
        Ok(Self {
            width,
            height,
            block,
            columns,
            rows,
            pairs,
            bits,
        })
    }

    pub fn bits_per_frame(&self) -> usize {
        self.bits
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

/// Prepares one frame for encoding: clamps the background luma, stamps the
/// anchor checkerboard onto the border blocks, and applies the pair
/// modulation for `bits` (row-major, most significant bit first). Each pair
/// is modulated around its own mean so a bit shifts both blocks in opposite
/// directions by exactly `MODULATION` regardless of the background content;
/// the decoder still just compares the block means. Data blocks past the
/// end of `bits` keep the clamped background.
pub fn modulate_at(image: &mut [u8], geometry: Geometry, dx: usize, dy: usize, bits: &[bool]) {
    let width = geometry.width;
    for value in &mut image[..width * geometry.height] {
        *value = (*value).clamp(BASE_LOW, BASE_HIGH);
    }
    let block = geometry.block;
    for row in 0..geometry.rows {
        for col in 0..geometry.columns {
            if geometry.is_anchor(col, row) {
                fill_block(
                    image,
                    block,
                    width,
                    col * block + dx,
                    row * block + dy,
                    anchor_value(col, row),
                );
            }
        }
    }
    let area = (block * block) as i64;
    let step = area * i64::from(MODULATION);
    for (index, &bit) in bits.iter().enumerate() {
        let row = index / geometry.pairs;
        let pair = index % geometry.pairs;
        let col = ANCHOR_COLS + 2 * pair;
        let x0 = col * block + dx;
        let x1 = (col + 1) * block + dx;
        let y0 = row * block + dy;
        let sum_a = i64::try_from(block_sum(image, block, width, x0, y0)).unwrap_or(0);
        let sum_b = i64::try_from(block_sum(image, block, width, x1, y0)).unwrap_or(0);
        let middle = (sum_a + sum_b) / 2;
        let (target_a, target_b) = if bit {
            (middle + step, middle - step)
        } else {
            (middle - step, middle + step)
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

pub fn modulate(image: &mut [u8], geometry: Geometry, bits: &[bool]) {
    modulate_at(image, geometry, 0, 0, bits);
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

/// Demodulates `out.len()` bytes from a frame: a bit is one when the mean
/// luma of the pair's first block is greater than the second block's.
/// `raster.scan` must have run on the same `y` plane first.
pub fn demodulate(geometry: Geometry, offset: (usize, usize), raster: &Raster, out: &mut [u8]) {
    let (dx, dy) = offset;
    let block = geometry.block;
    for (byte_index, byte) in out.iter_mut().enumerate() {
        let mut value = 0u8;
        for bit in 0..8 {
            let index = byte_index * 8 + bit;
            let row = index / geometry.pairs;
            let pair = index % geometry.pairs;
            let col = ANCHOR_COLS + 2 * pair;
            let a = raster.sum(col * block + dx, row * block + dy, block);
            let b = raster.sum((col + 1) * block + dx, row * block + dy, block);
            if a > b {
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
            &format!(
                "scale={}:{}:flags=bicubic,setsar=1",
                settings.width, settings.height
            ),
            "-r",
            &fps,
        ])
        .args(tools.frame_sync_cfr_args()?)
        .args(["-f", "rawvideo", "-pix_fmt", "yuv420p", "pipe:1"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped());
    let mut bg_process = tools.spawn(read_cmd)?;
    let bg_pipe = bg_process
        .child
        .stdout
        .take()
        .context("missing FFmpeg stdout")?;

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
    // Modulation runs on its own thread: while it modulates frame N and
    // feeds the encoder, this thread reads background frame N+1 and keeps
    // consuming the error-corrected payload stream.
    let (tx, rx) = mpsc::sync_channel::<(Vec<u8>, Vec<bool>)>(2);
    let (spare_tx, spare_rx) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        modulator(
            geometry,
            pipe,
            Progress::new("Encode", frames),
            rx,
            spare_tx,
        )
    });
    let mut writer = Writer {
        bg_pipe,
        geometry,
        image: vec![0; frame_len(geometry)],
        bits: Vec::with_capacity(geometry.bits_per_frame()),
        tx: Some(tx),
        spare_rx,
        worker: Some(worker),
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

struct Writer {
    bg_pipe: ChildStdout,
    geometry: Geometry,
    /// Frame currently being assembled on this thread.
    image: Vec<u8>,
    bits: Vec<bool>,
    /// Sends completed frames to the modulator thread; `None` after finish.
    tx: Option<mpsc::SyncSender<(Vec<u8>, Vec<bool>)>>,
    /// Returns modulated frames from the modulator thread for reuse.
    spare_rx: mpsc::Receiver<(Vec<u8>, Vec<bool>)>,
    worker: Option<std::thread::JoinHandle<Result<u64>>>,
}

/// The modulator thread: takes assembled frames, modulates them onto the
/// background image, and writes them to the encoder while the writers'
/// thread keeps reading background frames and the error-correction stream.
/// Returns the number of frames written.
fn modulator(
    geometry: Geometry,
    mut pipe: ChildStdin,
    mut progress: Progress,
    rx: mpsc::Receiver<(Vec<u8>, Vec<bool>)>,
    spare_tx: mpsc::Sender<(Vec<u8>, Vec<bool>)>,
) -> Result<u64> {
    let mut sent = 0u64;
    let mut bytes = 0u64;
    while let Ok((mut image, bits)) = rx.recv() {
        modulate(&mut image, geometry, &bits);
        pipe.write_all(&image)
            .context("could not send a modulated frame to FFmpeg")?;
        bytes += bits.len() as u64 / 8 + u64::from(bits.len() % 8 != 0);
        sent += 1;
        progress.update(sent, bytes);
        // Unbounded: this never blocks the encoder intake.
        let _ = spare_tx.send((image, bits));
    }
    Ok(sent)
}

impl Writer {
    fn push(&mut self, data: &[u8]) -> Result<()> {
        for &byte in data {
            for shift in (0..8).rev() {
                self.bit(byte >> shift & 1 == 1)?;
            }
        }
        Ok(())
    }

    fn bit(&mut self, one: bool) -> Result<()> {
        if self.bits.is_empty() {
            self.bg_pipe.read_exact(&mut self.image).map_err(|error| {
                anyhow::anyhow!("could not read the stealth background video: {error}")
            })?;
        }
        self.bits.push(one);
        if self.bits.len() == self.geometry.bits_per_frame() {
            self.cycle_frame()?;
        }
        Ok(())
    }

    /// Hands the completed frame to the modulator thread and replenishes the
    /// buffers from the recycle channel, keeping the buffers in-flight at
    /// two so the background read, the modulation and the encoder's intake
    /// all overlap.
    fn cycle_frame(&mut self) -> Result<()> {
        let image = std::mem::take(&mut self.image);
        let bits = std::mem::take(&mut self.bits);
        let tx = self
            .tx
            .as_ref()
            .context("the stealth modulator has already stopped")?;
        if let Err(_dropped) = tx.send((image, bits)) {
            return Err(self.modulator_failure());
        }
        match self.spare_rx.try_recv() {
            Ok((image, bits)) => {
                self.image = image;
                self.bits = bits;
                self.bits.clear();
            }
            Err(_) => {
                self.image = vec![0; frame_len(self.geometry)];
                self.bits = Vec::with_capacity(self.geometry.bits_per_frame());
            }
        }
        Ok(())
    }

    /// Recovers the modulator thread's result after it stopped early; the
    /// dropped channel was the symptom, the error it returned is the cause.
    fn modulator_failure(&mut self) -> anyhow::Error {
        self.tx = None;
        match self.worker.take() {
            Some(handle) => match handle.join() {
                Ok(Ok(_)) => anyhow::anyhow!("the stealth modulator stopped unexpectedly"),
                Ok(Err(error)) => error,
                Err(panic) => anyhow::anyhow!("the stealth modulator panicked: {panic:?}"),
            },
            None => anyhow::anyhow!("the stealth modulator stopped unexpectedly"),
        }
    }

    fn finish(&mut self) -> Result<u64> {
        if !self.bits.is_empty() {
            self.cycle_frame()?;
        }
        self.tx = None;
        match self.worker.take() {
            Some(handle) => handle
                .join()
                .map_err(|panic| anyhow::anyhow!("the stealth modulator panicked: {panic:?}"))
                .and_then(|sent| sent),
            None => Err(anyhow::anyhow!(
                "the stealth modulator stopped unexpectedly"
            )),
        }
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
    /// Opens a stealth stream without knowing the encoder's block size:
    /// every even candidate block that fits the resolution is demodulated
    /// from the incoming frames until the sync marker, transport header and
    /// the header's own block size all agree on one candidate. The default
    /// block is probed first, then the faster smaller ones, then the sturdier
    /// larger ones. Only the frames needed for detection are consumed.
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
            let Ok(geometry) = Geometry::new(width, height, block) else {
                continue;
            };
            let Some(at) = find_alignment(&image[..luma], geometry, &mut raster) else {
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
    use super::{Geometry, Raster, demodulate, find_alignment, modulate};

    const WIDTH: usize = 240;
    const HEIGHT: usize = 144;

    fn background() -> Vec<u8> {
        (0..WIDTH * HEIGHT * 3 / 2)
            .map(|i| (30 + (i * 37 + i / WIDTH * 11) % 221) as u8)
            .collect()
    }

    #[test]
    fn geometry_matches_the_documented_capacities() {
        let full = Geometry::new(1920, 1080, super::BLOCK).unwrap();
        assert_eq!(full.frame_bytes(), 3498);
        let small = Geometry::new(WIDTH, HEIGHT, super::BLOCK).unwrap();
        assert_eq!(small.frame_bytes(), 47);
        // Block 4 packs (6/4)^2 = 2.25x as many bit pairs into the same
        // frame; integer rounding gives 2.4x at this resolution.
        let dense = Geometry::new(WIDTH, HEIGHT, 4).unwrap();
        assert_eq!(dense.frame_bytes(), 116);
        assert!(
            dense.bits_per_frame() * 4 > small.bits_per_frame() * 9,
            "block 4 must carry at least the documented 2.25x density"
        );
        assert!(Geometry::new(24, 12, super::BLOCK).is_err());
    }

    #[test]
    fn modulation_round_trips_through_clamping() {
        // Both a default and a dense block must survive a full round trip.
        for block in [super::BLOCK, 4] {
            round_trip(block);
        }
    }

    fn round_trip(block: usize) {
        let geometry = Geometry::new(WIDTH, HEIGHT, block).unwrap();
        let bits: Vec<bool> = (0..geometry.frame_bytes() * 8)
            .map(|i| (i % 7) % 2 == 0)
            .collect();
        let mut image = background();
        modulate(&mut image, geometry, &bits);
        let mut raster = Raster::default();
        let alignment = find_alignment(&image[..WIDTH * HEIGHT], geometry, &mut raster);
        assert_eq!(alignment, Some((0, 0)));
        raster.scan(&image[..WIDTH * HEIGHT], WIDTH, HEIGHT);
        let mut out = vec![0; geometry.frame_bytes()];
        demodulate(geometry, alignment.unwrap(), &raster, &mut out);
        for (index, &bit) in bits.iter().enumerate() {
            assert_eq!(out[index / 8] >> (7 - index % 8) & 1 == 1, bit);
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
}
