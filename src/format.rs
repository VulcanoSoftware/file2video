use anyhow::{Result, ensure};
use std::path::Path;

pub const TRANSPORT_SIZE: usize = 64 * 7;
pub const ECC_SIZE: usize = 30;
pub const CRYPTO_SIZE: usize = 128;
pub const META_SIZE: usize = 22;
pub const MAX_NAME: usize = 4096;
pub const MAX_GCM: u64 = (1 << 36) - 32;
pub const MAX_KDF: u32 = 10_000_000;
pub const BLOCK_SIZES: [usize; 5] = [8, 10, 12, 16, 20];

#[derive(Clone, Copy, Debug)]
pub struct Grid {
    pub width: usize,
    pub height: usize,
    pub block: usize,
}

impl Grid {
    pub fn validate(self) -> Result<Self> {
        self.validate_layout()?;
        ensure!(
            self.bits() >= TRANSPORT_SIZE * 8,
            "frame too small for the Bit2Vid header; increase the resolution or reduce the block size"
        );
        Ok(self)
    }

    pub fn validate_layout(self) -> Result<Self> {
        validate_resolution(self.width, self.height)?;
        ensure!(BLOCK_SIZES.contains(&self.block), "unsupported block size");
        ensure!(
            self.width.is_multiple_of(self.block) && self.height.is_multiple_of(self.block),
            "width and height must be divisible by the block size"
        );
        Ok(self)
    }

    pub fn bits(self) -> usize {
        (self.width / self.block) * (self.height / self.block)
    }
}

pub fn validate_resolution(width: usize, height: usize) -> Result<()> {
    ensure!(
        (2..=8192).contains(&width)
            && (2..=8192).contains(&height)
            && width.is_multiple_of(2)
            && height.is_multiple_of(2)
            && width * height <= 8192 * 4320,
        "unsupported video dimensions: {width}x{height} (even dimensions, at most 8192 per side and 35.4 MP)"
    );
    Ok(())
}

pub fn u16_at(data: &[u8], at: usize) -> u16 {
    u16::from_be_bytes(data[at..at + 2].try_into().unwrap())
}

pub fn u32_at(data: &[u8], at: usize) -> u32 {
    u32::from_be_bytes(data[at..at + 4].try_into().unwrap())
}

pub fn u64_at(data: &[u8], at: usize) -> u64 {
    u64::from_be_bytes(data[at..at + 8].try_into().unwrap())
}

/// The video stores one cell per pixel at cell resolution instead of a full
/// resolution frame with block-scaled cells; the decoder upscales geometry.
pub const VIDEO_MIN_DIMENSION: usize = 256;

/// H.264 with yuv420p needs even video dimensions, and every hardware
/// encoder supports at least 256x256.
pub fn video_dimension(cells: usize) -> usize {
    cells.max(VIDEO_MIN_DIMENSION).next_multiple_of(2)
}

/// Parsed and majority-voted first bytes of the transport header.
pub struct Transport {
    pub width: usize,
    pub height: usize,
    pub block: usize,
    pub parity: usize,
    pub protected: u64,
    pub cell_bits: u8,
    pub compact: bool,
}

pub fn transport(
    grid: Grid,
    parity: usize,
    protected: u64,
    cell_bits: u8,
    compact: bool,
) -> [u8; TRANSPORT_SIZE] {
    let mut header = [0u8; 64];
    header[..8].copy_from_slice(b"CFVID001");
    header[8] = cell_bits;
    header[9] = u8::from(compact);
    for (i, n) in [grid.width, grid.height, grid.block, parity]
        .into_iter()
        .enumerate()
    {
        header[12 + i * 2..14 + i * 2].copy_from_slice(&(n as u16).to_be_bytes());
    }
    header[20..28].copy_from_slice(&protected.to_be_bytes());
    let mut out = [0u8; TRANSPORT_SIZE];
    out.as_chunks_mut::<64>()
        .0
        .iter_mut()
        .for_each(|copy| copy.copy_from_slice(&header));
    out
}

pub fn parse_transport(data: &[u8]) -> Result<Transport> {
    let mut vote = [0u8; 28];
    for (i, cell) in vote.iter_mut().enumerate() {
        for bit in 0..8 {
            let mask = 1 << bit;
            if data
                .as_chunks::<64>()
                .0
                .iter()
                .filter(|copy| copy[i] & mask != 0)
                .count()
                >= 4
            {
                *cell |= mask;
            }
        }
    }
    ensure!(&vote[..8] == b"CFVID001", "not a Bit2Vid v1 header");
    let cell_bits = vote[8];
    ensure!(
        cell_bits == 1 || cell_bits == 4,
        "unsupported cell width in the transport header"
    );
    let compact = vote[9] == 1;
    ensure!(vote[9] == 0 || compact, "unsupported transport mode");
    let width = usize::from(u16_at(&vote, 12));
    let height = usize::from(u16_at(&vote, 14));
    let block = usize::from(u16_at(&vote, 16));
    let parity = usize::from(u16_at(&vote, 18));
    ensure!(
        (1..=127).contains(&parity),
        "invalid Reed-Solomon parity count"
    );
    let protected = u64_at(&vote, 20);
    ensure!(
        protected > ECC_SIZE as u64 && (protected - ECC_SIZE as u64).is_multiple_of(255),
        "invalid protected payload length"
    );
    Ok(Transport {
        width,
        height,
        block,
        parity,
        protected,
        cell_bits,
        compact,
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Layout {
    pub encrypted: u64,
    pub words: u32,
    pub protected: u64,
}

impl Layout {
    pub fn new(encrypted: u64, parity: usize) -> Result<Self> {
        Self::build(encrypted, parity, false)
    }

    /// Compact layout: the 30-byte ECC header is the start of the first
    /// codeword's data area so Reed-Solomon corrects it; word sizing
    /// includes it. The transport header keeps the same protected length
    /// shape (30 + words * 255) in both modes.
    pub fn compact(encrypted: u64, parity: usize) -> Result<Self> {
        Self::build(encrypted, parity, true)
    }

    fn build(encrypted: u64, parity: usize, compact: bool) -> Result<Self> {
        ensure!((1..=127).contains(&parity), "parity must be in 1..=127");
        ensure!(
            (CRYPTO_SIZE as u64..=CRYPTO_SIZE as u64 + MAX_GCM).contains(&encrypted),
            "payload exceeds the Bit2Vid v1 AES-GCM size limit"
        );
        let data = if compact {
            encrypted + ECC_SIZE as u64
        } else {
            encrypted
        };
        let words = u32::try_from(data.div_ceil((255 - parity) as u64))?;
        Ok(Self {
            encrypted,
            words,
            protected: ECC_SIZE as u64 + u64::from(words) * 255,
        })
    }

    pub fn frames(&self, grid: Grid, cell_bits: u8) -> u64 {
        ((TRANSPORT_SIZE as u64 + self.protected) * 8)
            .div_ceil(grid.bits() as u64 * u64::from(cell_bits))
    }

    pub fn ecc_header(&self, parity: usize) -> [u8; ECC_SIZE] {
        let mut out = [0u8; ECC_SIZE];
        out[..9].copy_from_slice(b"CFECC001\x01");
        out[12..14].copy_from_slice(&(parity as u16).to_be_bytes());
        out[14..18].copy_from_slice(&((255 - parity) as u32).to_be_bytes());
        out[18..22].copy_from_slice(&self.words.to_be_bytes());
        out[22..30].copy_from_slice(&self.encrypted.to_be_bytes());
        out
    }

    pub fn parse(
        header: &[u8; ECC_SIZE],
        parity: usize,
        protected: u64,
        compact: bool,
    ) -> Result<Self> {
        let layout = Self::build(u64_at(header, 22), parity, compact)?;
        ensure!(
            layout.protected == protected && *header == layout.ecc_header(parity),
            "invalid or corrupted Reed-Solomon header"
        );
        Ok(layout)
    }
}

pub fn metadata(path: &Path, size: u64) -> Result<Vec<u8>> {
    let name = path
        .file_name()
        .and_then(|v| v.to_str())
        .ok_or_else(|| anyhow::anyhow!("the input filename must be valid UTF-8"))?;
    ensure!(
        !name.is_empty() && name.len() <= MAX_NAME,
        "invalid filename length"
    );
    ensure!(
        !name.contains(['/', '\\']),
        "filename cannot contain slashes"
    );
    let mut out = vec![0u8; META_SIZE];
    out[..9].copy_from_slice(b"CFMETA01\x01");
    out[12..14].copy_from_slice(&(name.len() as u16).to_be_bytes());
    out[14..22].copy_from_slice(&size.to_be_bytes());
    out.extend_from_slice(name.as_bytes());
    Ok(out)
}

// Called only on the first plaintext chunk; it holds the entire filename.
pub fn parse_metadata(first: &[u8], size: u64) -> Result<(Option<&str>, usize)> {
    if first.len() < META_SIZE || &first[..8] != b"CFMETA01" {
        return Ok((None, 0));
    }
    ensure!(first[8] == 1, "unsupported file metadata version");
    let n = usize::from(u16_at(first, 12));
    let offset = META_SIZE + n;
    ensure!(
        (1..=MAX_NAME).contains(&n)
            && offset <= first.len()
            && size - offset as u64 == u64_at(first, 14),
        "invalid file metadata lengths"
    );
    let name = std::str::from_utf8(&first[META_SIZE..offset])?;
    ensure!(
        !name.contains(['/', '\\']),
        "invalid path in stored filename"
    );
    Ok((Some(name), offset))
}

#[cfg(test)]
mod tests {
    use super::{ECC_SIZE, Layout};

    #[test]
    fn compact_layout_protects_the_ecc_header_in_word_zero() {
        let compact = Layout::compact(1910, 64).unwrap();
        let plain = Layout::new(1910, 64).unwrap();
        assert_eq!(compact.words, plain.words + 1);
        assert_eq!(
            compact.protected,
            ECC_SIZE as u64 + compact.words as u64 * 255
        );
        let header = compact.ecc_header(64);
        assert_eq!(
            Layout::parse(&header, 64, compact.protected, true).unwrap(),
            compact
        );
        assert!(Layout::parse(&header, 64, compact.protected, false).is_err());
        let plain_header = plain.ecc_header(64);
        assert_eq!(
            Layout::parse(&plain_header, 64, plain.protected, false).unwrap(),
            plain
        );
        assert!(Layout::parse(&plain_header, 64, plain.protected, true).is_err());
    }
}
