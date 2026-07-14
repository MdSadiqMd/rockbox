//! Shared sha256 helpers. Keeping them here avoids three crates each pulling
//! in `sha2` and disagreeing on hash encoding

use sha2::{Digest, Sha256};
use std::fmt;

pub const DIGEST_LEN: usize = 32;

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Sha256Digest(pub [u8; DIGEST_LEN]);

impl Sha256Digest {
    pub fn from_bytes(bytes: &[u8]) -> Self {
        let mut h = Sha256::new();
        h.update(bytes);
        Self(h.finalize().into())
    }

    pub fn parse_hex(s: &str) -> Result<Self, hex::FromHexError> {
        let mut out = [0u8; DIGEST_LEN];
        hex::decode_to_slice(s, &mut out)?;
        Ok(Self(out))
    }

    pub fn to_hex(self) -> String {
        hex::encode(self.0)
    }
}

impl fmt::Debug for Sha256Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Sha256Digest").field(&self.to_hex()).finish()
    }
}

impl fmt::Display for Sha256Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.to_hex())
    }
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256Digest::from_bytes(bytes).to_hex()
}
