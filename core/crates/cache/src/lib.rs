//! Caches owned by the engine process:
//!
//! - **T1 env cache** ([`env_cache`]): pre-warmed Nix env-var sets keyed by
//!   `sha256(flake.lock + lang + arch + glibc_ver)`. Top-N kept in mmap'd
//!   memfd (PERF-09) on Linux; on darwin we fall back to plain process memory.

pub mod env_cache;
pub mod hash;

pub use env_cache::{EnvCache, EnvKey, EnvSnapshot};
pub use hash::{Sha256Digest, sha256_hex};
