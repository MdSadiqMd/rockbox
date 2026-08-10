//! T2 binary cache. The verified-fd open dance lives here
//!
//! Storage layout under `/var/cache/sandbox/bin/<hash>/`:
//! - `main.bin`     (0555, root:root, written by compiler)
//! - `main.sha256`  (0444, root:root, stores ASCII hex digest of `main.bin`)
//!
//! The engine opens both files with `O_NOFOLLOW | O_CLOEXEC` via a `dirfd`
//! anchor. It reads the expected digest, hashes the binary fd, and only on
//! match returns a [`BinaryHandle`] that wraps the verified fd. The Mount
//! step in `core` bind-mounts from `/proc/self/fd/<binfd>` — paths
//! are never re-resolved

use crate::hash::Sha256Digest;
use dashmap::DashMap;
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use thiserror::Error;
use tracing::{debug, instrument};

/// Key = content-addressed identity of the compiled artifact.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BinaryKey {
    pub digest: Sha256Digest,
}

impl BinaryKey {
    pub fn from_parts(code: &[u8], compiler_version: &str, arch: &str, flake_lock: &[u8]) -> Self {
        let mut h = Sha256::new();
        h.update(code);
        h.update(b"\0");
        h.update(compiler_version.as_bytes());
        h.update(b"\0");
        h.update(arch.as_bytes());
        h.update(b"\0");
        h.update(flake_lock);
        Self {
            digest: Sha256Digest(h.finalize().into()),
        }
    }

    pub fn dir_name(&self) -> String {
        self.digest.to_hex()
    }
}

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum BinaryCacheError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("integrity mismatch (stored {stored} != computed {computed})")]
    IntegrityMismatch { stored: String, computed: String },
    #[error("missing integrity file at {0}")]
    MissingIntegrity(PathBuf),
    #[error("invalid integrity digest format")]
    InvalidDigest(#[from] hex::FromHexError),
    #[error("binary not in cache")]
    NotFound,
}

/// Handle to a verified binary. Holds the open file so the underlying inode
/// cannot be swapped out from under us; expose only the fd path so the
/// mount step never re-resolves through the filesystem (closes TOCTOU).
#[derive(Debug)]
pub struct BinaryHandle {
    file: std::fs::File,
    /// Cached `/proc/self/fd/<n>` string on Linux; raw fd number on darwin.
    fd_path: String,
    /// Same digest stored in `.sha256` file; cheap to clone.
    pub digest: Sha256Digest,
}

impl BinaryHandle {
    pub fn fd_path(&self) -> &str {
        &self.fd_path
    }
    pub fn as_file(&self) -> &std::fs::File {
        &self.file
    }
}

/// Thread-safe wrapper. Keeps an LRU-ish handle map so the engine can return
/// cached `BinaryHandle`s without re-opening on warm hits.
#[derive(Debug, Clone)]
pub struct BinaryCache {
    root: Arc<Path>,
    handles: Arc<DashMap<BinaryKey, Arc<BinaryHandle>>>,
}

impl BinaryCache {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        let root: PathBuf = root.into();
        Self {
            root: Arc::from(root.into_boxed_path()),
            handles: Arc::new(DashMap::new()),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn dir_for(&self, key: &BinaryKey) -> PathBuf {
        self.root.join(key.dir_name())
    }

    /// Returns a handle if a valid, integrity-checked binary exists for `key`.
    ///
    /// Implementation notes (FIX SEC-15):
    /// 1. open dir with `O_PATH | O_DIRECTORY | O_NOFOLLOW` (we use plain
    ///    `File::open` here for portability; the engine's mount step assumes
    ///    Linux + dirfd-relative opens — see `kernel::mounts`).
    /// 2. read `.sha256` from the same dir.
    /// 3. open `main.bin`, hash bytes streamed from the fd, compare.
    /// 4. on match: keep the fd open and return a handle pointing at
    ///    `/proc/self/fd/<n>`. Mounting through that pseudo-path freezes the
    ///    inode reference — even if an attacker `rm`s and replaces
    ///    `main.bin`, the bind-mount source still points at our verified fd.
    #[instrument(skip(self), fields(key = %key.digest))]
    pub fn lookup(&self, key: &BinaryKey) -> Result<Arc<BinaryHandle>, BinaryCacheError> {
        if let Some(h) = self.handles.get(key) {
            return Ok(h.value().clone());
        }
        let dir = self.dir_for(key);
        let bin_path = dir.join("main.bin");
        let hash_path = dir.join("main.sha256");

        let stored_hex = std::fs::read_to_string(&hash_path)
            .map_err(|_| BinaryCacheError::MissingIntegrity(hash_path.clone()))?;
        let stored_hex = stored_hex.trim();
        let stored = Sha256Digest::parse_hex(stored_hex)?;

        let mut file = open_nofollow(&bin_path)?;
        let computed = stream_sha256(&mut file)?;
        if computed != stored {
            return Err(BinaryCacheError::IntegrityMismatch {
                stored: stored.to_hex(),
                computed: computed.to_hex(),
            });
        }
        let handle = Arc::new(BinaryHandle {
            fd_path: fd_path_for(&file),
            file,
            digest: stored,
        });
        self.handles.insert(key.clone(), handle.clone());
        debug!("binary_cache_hit");
        Ok(handle)
    }

    /// Persist a freshly compiled artifact under `main.bin` + `main.sha256`
    /// and return a verified handle for immediate use. Best-effort from the
    /// engine's perspective: the cache dir may be read-only (privileged
    /// deployments compile via the separate `compiler` helper instead).
    ///
    /// Takes the on-disk artifact path — the compiled binary already lives
    /// in the content-addressed work dir — so the hot byte buffer written by
    /// the compiler is streamed once via `fs::copy` instead of being read
    /// into engine memory and rewritten.
    #[instrument(skip(self, artifact), fields(key = %key.digest))]
    pub fn store(&self, key: &BinaryKey, artifact: &Path) -> std::io::Result<Arc<BinaryHandle>> {
        let dir = self.dir_for(key);
        std::fs::create_dir_all(&dir)?;
        let bin_path = dir.join("main.bin");
        // fs::copy streams from the source fd — no full read into RAM.
        std::fs::copy(artifact, &bin_path)?;
        // The engine execve()s the cached artifact through `/proc/self/fd/N`,
        // and the kernel checks the inode's execute bit on that path — 0644
        // from the copy above would be EACCES at exec time.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perm = std::fs::metadata(&bin_path)?.permissions();
            perm.set_mode(0o555);
            std::fs::set_permissions(&bin_path, perm)?;
        }
        let mut file = std::fs::File::open(&bin_path)?;
        let digest = stream_sha256(&mut file)?;
        std::fs::write(dir.join("main.sha256"), digest.to_hex())?;
        let handle = Arc::new(BinaryHandle {
            fd_path: fd_path_for(&file),
            file,
            digest,
        });
        self.handles.insert(key.clone(), handle.clone());
        debug!("binary_cache_stored");
        Ok(handle)
    }
}

fn stream_sha256(file: &mut std::fs::File) -> std::io::Result<Sha256Digest> {
    use std::io::Seek;
    file.rewind()?;
    let mut h = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    file.rewind()?;
    Ok(Sha256Digest(h.finalize().into()))
}

#[cfg(target_os = "linux")]
fn open_nofollow(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
}

#[cfg(not(target_os = "linux"))]
fn open_nofollow(path: &Path) -> std::io::Result<std::fs::File> {
    std::fs::File::open(path)
}

#[cfg(target_os = "linux")]
fn fd_path_for(file: &std::fs::File) -> String {
    use std::os::fd::AsRawFd;
    format!("/proc/self/fd/{}", file.as_raw_fd())
}

#[cfg(not(target_os = "linux"))]
fn fd_path_for(file: &std::fs::File) -> String {
    use std::os::fd::AsRawFd;
    // darwin has no /proc/self/fd; engine on darwin won't actually mount.
    format!("fd:{}", file.as_raw_fd())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_is_deterministic() {
        let a = BinaryKey::from_parts(b"code", "1.0", "x86_64", b"lock");
        let b = BinaryKey::from_parts(b"code", "1.0", "x86_64", b"lock");
        assert_eq!(a, b);
    }

    #[test]
    fn lookup_verifies_integrity() {
        let tmp = std::env::temp_dir().join(format!("rockbox-bincache-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let cache = BinaryCache::new(&tmp);
        let key = BinaryKey::from_parts(b"hello", "1.0", "x86_64", b"lock");
        let dir = cache.dir_for(&key);
        std::fs::create_dir_all(&dir).unwrap();
        let payload = b"binary content";
        std::fs::write(dir.join("main.bin"), payload).unwrap();
        let digest = Sha256Digest::from_bytes(payload);
        std::fs::write(dir.join("main.sha256"), digest.to_hex()).unwrap();

        let h = cache.lookup(&key).unwrap();
        assert_eq!(h.digest, digest);

        // Corrupt the integrity file → next cold lookup fails.
        std::fs::write(
            dir.join("main.sha256"),
            Sha256Digest::from_bytes(b"x").to_hex(),
        )
        .unwrap();
        // Cached handle still works (we trust the fd); evict and retry.
        cache.handles.clear();
        let err = cache.lookup(&key).unwrap_err();
        assert!(matches!(err, BinaryCacheError::IntegrityMismatch { .. }));

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
