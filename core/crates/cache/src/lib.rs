//! Caches owned by the engine process:
//!
//! - **T1 env cache** ([`env_cache`]): pre-warmed Nix env-var sets keyed by
//!   `sha256(flake.lock + lang + arch + glibc_ver)`. Top-N kept in mmap'd
//!   memfd (PERF-09) on Linux; on darwin we fall back to plain process memory.
//!
//! - **T2 binary cache** ([`binary_cache`]): compiled user-code binaries
//!   keyed by `sha256(code + compiler_ver + arch + flake.lock)` (PERF-15).
//!   The verified-fd open dance (SEC-15) is implemented here so the engine
//!   never re-resolves the path after `sha256(fd) == stored_hash`.

// Unsafe is used only in `env_cache::backing` for memfd_create + mmap on Linux.
// All other modules are unsafe-free.

pub mod binary_cache;
pub mod env_cache;
pub mod hash;

pub use binary_cache::{BinaryCache, BinaryHandle, BinaryKey};
pub use env_cache::{EnvCache, EnvKey, EnvSnapshot};
pub use hash::{Sha256Digest, sha256_hex};
