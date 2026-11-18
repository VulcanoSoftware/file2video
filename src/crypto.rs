//! Streaming adapter for Bit2Vid v1 (one AES-GCM message per file, no AAD).
//! Crypto primitives are RustCrypto. Only the final update may be unaligned.
use aes::Aes256;
use aes::cipher::{BlockEncrypt, InnerIvInit, KeyInit, StreamCipher};
use anyhow::{Result, bail, ensure};
use ghash::GHash;
use ghash::universal_hash::UniversalHash;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use zeroize::Zeroizing;

use crate::format::{CRYPTO_SIZE, MAX_GCM, MAX_KDF, u32_at, u64_at};

const BLOCK: usize = 16;

pub struct Header {
    pub iterations: u32,
    pub salt: [u8; 16],
    pub nonce: [u8; 12],
    pub size: u64,
    pub tag: [u8; 16],
    pub hash: [u8; 32],
}

impl Header {
    pub fn new(iterations: u32, size: u64) -> Result<Self> {
        ensure!(
            (1..=MAX_KDF).contains(&iterations),
            "invalid PBKDF2 iterations"
        );
        ensure!(
            size <= MAX_GCM,
            "file exceeds the Bit2Vid v1 AES-GCM size limit"
        );
        let mut salt = [0u8; 16];
        let mut nonce = [0u8; 12];
        getrandom::fill(&mut salt).map_err(|e| anyhow::anyhow!("random salt: {e}"))?;
        getrandom::fill(&mut nonce).map_err(|e| anyhow::anyhow!("random nonce: {e}"))?;
        Ok(Self {
            iterations,
            salt,
            nonce,
            size,
            tag: [0; 16],
            hash: [0; 32],
        })
    }

    pub fn bytes(&self) -> [u8; CRYPTO_SIZE] {
        let mut out = [0u8; CRYPTO_SIZE];
        out[..9].copy_from_slice(b"CFVLT001\x01");
        out[12..16].copy_from_slice(&self.iterations.to_be_bytes());
        out[16..32].copy_from_slice(&self.salt);
        out[32..44].copy_from_slice(&self.nonce);
        out[44..60].copy_from_slice(&self.tag);
        out[60..68].copy_from_slice(&self.size.to_be_bytes());
        out[68..100].copy_from_slice(&self.hash);
        out
    }

    pub fn parse(bytes: &[u8; CRYPTO_SIZE], size: u64) -> Result<Self> {
        ensure!(&bytes[..9] == b"CFVLT001\x01", "invalid encryption header");
        let iterations = u32_at(bytes, 12);
        ensure!(
            (1..=MAX_KDF).contains(&iterations),
            "invalid PBKDF2 iterations"
        );
        ensure!(
            size <= MAX_GCM && u64_at(bytes, 60) == size,
            "invalid ciphertext length"
        );
        Ok(Self {
            iterations,
            salt: bytes[16..32].try_into().unwrap(),
            nonce: bytes[32..44].try_into().unwrap(),
            size,
            tag: bytes[44..60].try_into().unwrap(),
            hash: bytes[68..100].try_into().unwrap(),
        })
    }
}

pub fn password_bytes(password: &str) -> Result<Zeroizing<Vec<u8>>> {
    ensure!(!password.is_empty(), "password cannot be empty");
    let mut bytes = Zeroizing::new(Vec::with_capacity(password.len()));
    for ch in password.chars() {
        // PyCryptodome's PBKDF2 encodes Python str using Latin-1, not UTF-8.
        match u8::try_from(u32::from(ch)) {
            Ok(byte) => bytes.push(byte),
            Err(_) => bail!("Bit2Vid v1 passwords must use Latin-1 characters"),
        }
    }
    Ok(bytes)
}

pub struct Stream {
    ctr: ctr::Ctr32BE<Aes256>,
    ghash: GHash,
    mask: [u8; BLOCK],
    digest: Sha256,
    left: u64,
    size: u64,
}

impl Stream {
    pub fn new(header: &Header, password: &[u8]) -> Self {
        let mut key = Zeroizing::new([0u8; 32]);
        pbkdf2::pbkdf2_hmac::<Sha256>(password, &header.salt, header.iterations, &mut *key);
        Self::with_key(header.nonce, header.size, &key)
    }

    fn with_key(nonce: [u8; 12], size: u64, key: &[u8; 32]) -> Self {
        let aes = Aes256::new(key.into());
        let mut h = [0u8; BLOCK];
        aes.encrypt_block((&mut h).into());
        let mut counter = [0u8; BLOCK];
        counter[..12].copy_from_slice(&nonce);
        counter[15] = 1; // J0 for the 96-bit nonce defined by Bit2Vid v1.
        let mut mask = counter;
        aes.encrypt_block((&mut mask).into());
        counter[15] = 2; // First message block uses inc32(J0).
        Self {
            ctr: ctr::Ctr32BE::from_core(ctr::CtrCore::inner_iv_init(aes, (&counter).into())),
            ghash: GHash::new((&h).into()),
            mask,
            digest: Sha256::new(),
            left: size,
            size,
        }
    }

    pub fn update(&mut self, data: &mut [u8], encrypt: bool) -> Result<()> {
        let n = data.len() as u64;
        ensure!(
            n <= self.left && (n == self.left || data.len().is_multiple_of(BLOCK)),
            "internal crypto streaming length error"
        );
        if encrypt {
            self.digest.update(&*data);
            self.ctr.apply_keystream(data);
            self.ghash.update_padded(data);
        } else {
            self.ghash.update_padded(data);
            self.ctr.apply_keystream(data);
            self.digest.update(&*data);
        }
        self.left -= n;
        Ok(())
    }

    pub fn finish(mut self) -> Result<([u8; BLOCK], [u8; 32])> {
        ensure!(self.left == 0, "truncated ciphertext");
        let mut lengths = [0u8; BLOCK];
        lengths[8..].copy_from_slice(&(self.size * 8).to_be_bytes());
        self.ghash.update_padded(&lengths);
        for (mask, g) in self.mask.iter_mut().zip(self.ghash.finalize()) {
            *mask ^= g;
        }
        Ok((self.mask, self.digest.finalize().into()))
    }

    pub fn verify(self, header: &Header) -> Result<()> {
        let (tag, hash) = self.finish()?;
        ensure!(
            bool::from(tag.ct_eq(&header.tag) & hash.ct_eq(&header.hash)),
            "authentication failed: wrong password or damaged video"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aes_gcm::{Aes256Gcm, aead::AeadInPlace};

    #[test]
    fn matches_reference_across_chunk_boundaries() -> Result<()> {
        let key = [0x27; 32];
        let nonce = [0x13; 12];
        let cipher = Aes256Gcm::new((&key).into());
        for n in [0, 1, 15, 16, 17, 31, 64, 1023, 1024, 1025, 65537] {
            let plain: Vec<u8> = (0..n).map(|i| (i * 113) as u8).collect();
            let mut data = plain.clone();
            let mut expected = plain.clone();
            let expected_tag = cipher
                .encrypt_in_place_detached((&nonce).into(), b"", &mut expected)
                .unwrap();
            let mut enc = Stream::with_key(nonce, n, &key);
            for chunk in data.chunks_mut(64) {
                enc.update(chunk, true)?;
            }
            let (tag, hash) = enc.finish()?;
            assert_eq!(data, expected);
            assert_eq!(tag.as_slice(), expected_tag.as_slice());
            let mut dec = Stream::with_key(nonce, n, &key);
            for chunk in data.chunks_mut(32) {
                dec.update(chunk, false)?;
            }
            assert_eq!(data, plain);
            assert_eq!(dec.finish()?, (tag, hash));
        }
        Ok(())
    }
}
