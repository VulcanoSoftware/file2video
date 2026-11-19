//! Systematic RS(255,k), GF(256) polynomial 0x11d, roots alpha^0..alpha^(parity-1).
use anyhow::{Result, ensure};
use rayon::prelude::*;
use std::io::Read;

const WORD: usize = 255;
const STATE: usize = 128;
const BATCH: usize = 4096;
type Row = [u8; STATE];
type Kernel = fn(&[u8], &[Row], &mut Row);

fn mul(mut a: u8, mut b: u8) -> u8 {
    let mut out = 0;
    while b != 0 {
        if b & 1 != 0 {
            out ^= a;
        }
        a = (a << 1) ^ if a & 128 != 0 { 0x1d } else { 0 };
        b >>= 1;
    }
    out
}

fn portable<const N: usize>(data: &[u8], table: &[Row], result: &mut Row) {
    let mut state = [0u8; STATE + 1];
    for &byte in data {
        let row = &table[usize::from(byte ^ state[0])];
        for i in 0..N {
            state[i] = state[i + 1] ^ row[i];
        }
    }
    result[..N].copy_from_slice(&state[..N]);
}

#[cfg(target_arch = "x86_64")]
mod simd {
    use super::*;
    use std::arch::x86_64::*;

    pub fn avx2<const N: usize>(data: &[u8], table: &[Row], result: &mut Row) {
        // Only selected after the runtime feature check in Codec::new.
        unsafe { avx2_impl::<N>(data, table, result) };
    }

    #[target_feature(enable = "avx2")]
    unsafe fn avx2_impl<const N: usize>(data: &[u8], table: &[Row], result: &mut Row) {
        let mut state = [0u8; STATE + 1];
        for &byte in data {
            let row = &table[usize::from(byte ^ state[0])];
            // Each forward load precedes its overlapping store. The sentinel
            // byte stays zero and all unaligned 32-byte accesses are in bounds.
            unsafe {
                let s = state.as_mut_ptr();
                for i in (0..N).step_by(32) {
                    let a = _mm256_loadu_si256(s.add(i + 1).cast());
                    let b = _mm256_loadu_si256(row.as_ptr().add(i).cast());
                    _mm256_storeu_si256(s.add(i).cast(), _mm256_xor_si256(a, b));
                }
            }
        }
        result[..N].copy_from_slice(&state[..N]);
    }
}

pub struct Codec {
    parity: usize,
    table: Vec<Row>,
    kernel: Kernel,
    decoder: reed_solomon::Decoder,
}

impl Codec {
    pub fn new(parity: usize) -> Self {
        let mut generator = [0u8; STATE];
        generator[0] = 1;
        let mut root = 1;
        for degree in 1..=parity {
            for j in (1..=degree).rev() {
                generator[j] ^= mul(generator[j - 1], root);
            }
            root = mul(root, 2);
        }
        let mut table = vec![[0u8; STATE]; 256];
        for (factor, row) in table.iter_mut().enumerate() {
            for (cell, &coefficient) in row.iter_mut().zip(&generator[1..=parity]) {
                *cell = mul(factor as u8, coefficient);
            }
        }
        let kernel: Kernel = match parity {
            1..=16 => portable::<16>,
            17..=32 => portable::<32>,
            33..=64 => portable::<64>,
            _ => portable::<128>,
        };
        #[cfg(target_arch = "x86_64")]
        let kernel = if std::arch::is_x86_feature_detected!("avx2") {
            match parity {
                1..=32 => simd::avx2::<32>,
                33..=64 => simd::avx2::<64>,
                _ => simd::avx2::<128>,
            }
        } else {
            kernel
        };
        Self {
            parity,
            table,
            kernel,
            decoder: reed_solomon::Decoder::new(parity),
        }
    }

    fn k(&self) -> usize {
        WORD - self.parity
    }

    fn parity(&self, data: &[u8]) -> Row {
        let mut r = [0u8; STATE];
        (self.kernel)(data, &self.table, &mut r);
        r
    }

    fn encode_batch(&self, plain: &[u8], out: &mut [u8]) {
        out.par_chunks_mut(WORD)
            .zip(plain.par_chunks(self.k()))
            .with_min_len(128)
            .for_each(|(word, data)| {
                let (stored, parity) = word.split_at_mut(self.k());
                stored[..data.len()].copy_from_slice(data);
                stored[data.len()..].fill(0);
                parity.copy_from_slice(&self.parity(stored)[..self.parity]);
            });
    }

    pub fn encode(
        &self,
        mut input: impl Read,
        mut remaining: u64,
        mut emit: impl FnMut(&[u8]) -> Result<()>,
    ) -> Result<()> {
        let mut plain = vec![0; self.k() * BATCH];
        let mut coded = vec![0; WORD * BATCH];
        while remaining > 0 {
            let n = remaining.min(plain.len() as u64) as usize;
            input.read_exact(&mut plain[..n])?;
            let out = &mut coded[..n.div_ceil(self.k()) * WORD];
            self.encode_batch(&plain[..n], out);
            emit(out)?;
            remaining -= n as u64;
        }
        Ok(())
    }

    fn decode_word(&self, word: &[u8], plain: &mut [u8]) -> Result<()> {
        let (data, parity) = word.split_at(self.k());
        if self.parity(data)[..self.parity] == *parity {
            plain.copy_from_slice(data);
        } else {
            let fixed = self.decoder.correct(word, None).map_err(|_| {
                anyhow::anyhow!("Reed-Solomon recovery failed: too many damaged symbols")
            })?;
            plain.copy_from_slice(fixed.data());
        }
        Ok(())
    }

    // Grow the read and recovery buffers once; the caller can consume aligned
    // AES chunks regardless of the RS word size.
    pub fn reader<R: Read>(&self, input: R, size: u64) -> Reader<'_, R> {
        Reader {
            codec: self,
            input,
            remaining: size,
            coded: vec![0; WORD * BATCH],
            plain: vec![0; self.k() * BATCH],
            pos: 0,
            end: 0,
        }
    }
}

pub struct Reader<'a, R> {
    codec: &'a Codec,
    input: R,
    remaining: u64,
    coded: Vec<u8>,
    plain: Vec<u8>,
    pos: usize,
    end: usize,
}

impl<'a, R> Reader<'a, R> {
    // Lets the caller adjust the underlying source (frame progress display)
    // after the reader already borrows it for the payload reads.
    pub fn input_mut(&mut self) -> &mut R {
        &mut self.input
    }
}

impl<R: Read> Reader<'_, R> {
    pub fn read_exact(&mut self, mut out: &mut [u8]) -> Result<()> {
        while !out.is_empty() {
            if self.pos == self.end {
                ensure!(self.remaining != 0, "truncated ECC payload");
                let k = self.codec.k();
                let n = self.remaining.min(self.plain.len() as u64) as usize;
                let words = n.div_ceil(k);
                let coded = &mut self.coded[..words * WORD];
                self.input.read_exact(coded)?;
                self.plain[..words * k]
                    .par_chunks_mut(k)
                    .zip(coded.par_chunks(WORD))
                    .with_min_len(128)
                    .try_for_each(|(plain, word)| self.codec.decode_word(word, plain))?;
                self.remaining -= n as u64;
                self.pos = 0;
                self.end = n;
            }
            let n = out.len().min(self.end - self.pos);
            out[..n].copy_from_slice(&self.plain[self.pos..self.pos + n]);
            self.pos += n;
            out = &mut out[n..];
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parity_matches_reference_for_every_setting() {
        for p in 1..=127 {
            let codec = Codec::new(p);
            let input: Vec<u8> = (0..codec.k() - 3)
                .map(|i| (i * i + 131 * p) as u8)
                .collect();
            let mut padded = input.clone();
            padded.resize(codec.k(), 0);
            let mut actual = [0u8; WORD];
            codec.encode_batch(&input, &mut actual);
            let reference = reed_solomon::Encoder::new(p).encode(&padded);
            assert_eq!(&actual, &reference[..], "parity {p}");
            let mut scalar = [0; STATE];
            portable::<128>(&padded, &codec.table, &mut scalar);
            assert_eq!(&scalar[..p], reference.ecc());
        }
    }

    #[test]
    fn repairs_32_bad_symbols_per_codeword() -> Result<()> {
        let codec = Codec::new(64);
        for offset in 0..8 {
            let data: Vec<u8> = (0..codec.k()).map(|i| (89 * i + offset) as u8).collect();
            let mut word = [0u8; WORD];
            codec.encode_batch(&data, &mut word);
            for i in 0..32 {
                word[(i * 13 + offset) % WORD] ^= (i + 1) as u8;
            }
            let mut recovered = vec![0; codec.k()];
            codec.decode_word(&word, &mut recovered)?;
            assert_eq!(data, recovered);
        }
        Ok(())
    }

    #[test]
    #[ignore = "manual throughput measurement: cargo test --release ecc_throughput -- --ignored --nocapture"]
    fn ecc_throughput() -> Result<()> {
        let codec = Codec::new(64);
        let size = 128 * 1024 * 1024;
        let input = (0..size)
            .scan(123456789u32, |s, _| {
                *s ^= *s << 13;
                *s ^= *s >> 17;
                *s ^= *s << 5;
                Some(*s as u8)
            })
            .collect::<Vec<_>>();
        let start = std::time::Instant::now();
        codec.encode(input.as_slice(), size, |chunk| {
            std::hint::black_box(chunk);
            Ok(())
        })?;
        let seconds = start.elapsed().as_secs_f64();
        eprintln!(
            "128 MiB in {seconds:.3}s ({:.1} MiB/s; {} words/s)",
            128.0 / seconds,
            (size.div_ceil(191) as f64 / seconds) as u64
        );
        Ok(())
    }
}
