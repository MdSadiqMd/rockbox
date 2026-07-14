//! T1 env cache. Stores serialised env-var blobs keyed by
//! `sha256(flake.lock + lang + arch + glibc_ver)`
//!
//! On Linux the hot path stores the blob in an `memfd_create`'d region and
//! returns an `&[u8]` slice via mmap (FIX PERF-09 — zero syscalls on lookup).
//! On darwin (dev) we just stash `Arc<Vec<u8>>` in process memory

use crate::hash::Sha256Digest;
use dashmap::DashMap;
use once_cell::sync::OnceCell;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use sha2::Digest;
use std::collections::VecDeque;
use std::sync::Arc;
use thiserror::Error;
use tracing::{debug, instrument};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct EnvKey {
    pub digest: Sha256Digest,
}

impl EnvKey {
    pub fn from_parts(flake_lock: &[u8], language: &str, arch: &str, glibc_ver: &str) -> Self {
        let mut h = sha2::Sha256::new();
        h.update(flake_lock);
        h.update(b"\0");
        h.update(language.as_bytes());
        h.update(b"\0");
        h.update(arch.as_bytes());
        h.update(b"\0");
        h.update(glibc_ver.as_bytes());
        Self {
            digest: Sha256Digest(h.finalize().into()),
        }
    }
}

/// One serialised env snapshot. Format is line-oriented `KEY=VALUE\n` UTF-8.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnvSnapshot {
    pub vars: Vec<(String, String)>,
}

impl EnvSnapshot {
    pub fn to_blob(&self) -> Vec<u8> {
        let mut out =
            Vec::with_capacity(self.vars.iter().map(|(k, v)| k.len() + v.len() + 2).sum());
        for (k, v) in &self.vars {
            out.extend_from_slice(k.as_bytes());
            out.push(b'=');
            out.extend_from_slice(v.as_bytes());
            out.push(b'\n');
        }
        out
    }

    pub fn parse(blob: &[u8]) -> Self {
        let mut vars = Vec::new();
        for line in blob.split(|&b| b == b'\n') {
            if line.is_empty() {
                continue;
            }
            if let Some(eq) = line.iter().position(|&b| b == b'=') {
                let k = String::from_utf8_lossy(&line[..eq]).into_owned();
                let v = String::from_utf8_lossy(&line[eq + 1..]).into_owned();
                vars.push((k, v));
            }
        }
        Self { vars }
    }
}

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum EnvCacheError {
    #[error("env snapshot not found for key {0}")]
    NotFound(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// In-process cache. Thread-safe; cheap to clone (`Arc` inside).
#[derive(Debug, Default, Clone)]
pub struct EnvCache {
    inner: Arc<EnvCacheInner>,
}

#[derive(Debug, Default)]
struct EnvCacheInner {
    map: DashMap<EnvKey, Arc<EnvBlob>>,
    /// LRU ordering for eviction beyond `capacity`.
    lru: Mutex<VecDeque<EnvKey>>,
    capacity: usize,
}

#[derive(Debug)]
pub struct EnvBlob {
    bytes: backing::Backing,
}

impl EnvBlob {
    pub fn as_slice(&self) -> &[u8] {
        self.bytes.as_slice()
    }
}

impl EnvCache {
    pub fn new(capacity: usize) -> Self {
        let cap = capacity.max(1);
        Self {
            inner: Arc::new(EnvCacheInner {
                map: DashMap::new(),
                lru: Mutex::new(VecDeque::with_capacity(cap)),
                capacity: cap,
            }),
        }
    }

    /// Global singleton used by the engine main loop. `OnceCell::get_or_init`
    /// is fine here because the capacity is fixed at engine start.
    pub fn global() -> &'static Self {
        static G: OnceCell<EnvCache> = OnceCell::new();
        G.get_or_init(|| Self::new(default_capacity()))
    }

    #[instrument(skip(self, blob), fields(key = %key.digest))]
    pub fn insert(&self, key: EnvKey, blob: Vec<u8>) -> Result<Arc<EnvBlob>, EnvCacheError> {
        let backed = backing::Backing::from_vec(blob)?;
        let entry = Arc::new(EnvBlob { bytes: backed });
        self.inner.map.insert(key.clone(), entry.clone());
        self.touch(key);
        Ok(entry)
    }

    pub fn get(&self, key: &EnvKey) -> Option<Arc<EnvBlob>> {
        let hit = self.inner.map.get(key).map(|r| r.value().clone());
        if hit.is_some() {
            self.touch(key.clone());
        }
        hit
    }

    fn touch(&self, key: EnvKey) {
        let mut lru = self.inner.lru.lock();
        if let Some(pos) = lru.iter().position(|k| k == &key) {
            lru.remove(pos);
        }
        lru.push_back(key);
        while lru.len() > self.inner.capacity {
            if let Some(victim) = lru.pop_front() {
                self.inner.map.remove(&victim);
                debug!(key = %victim.digest, "env_cache_evicted");
            }
        }
    }
}

fn default_capacity() -> usize {
    std::env::var("ROCKBOX_ENV_CACHE_CAP")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(16)
}

// Backing: Linux memfd vs darwin Vec
#[cfg(target_os = "linux")]
mod backing {
    use memmap2::{Mmap, MmapOptions};
    use std::fs::File;
    use std::os::fd::FromRawFd;

    pub(super) struct Backing {
        mmap: Mmap,
        // Owner lives as long as the mmap so the fd remains valid until both drop.
        _owner: File,
    }

    impl Backing {
        pub(super) fn from_vec(data: Vec<u8>) -> std::io::Result<Self> {
            // SAFETY: memfd_create is a stable Linux syscall; the returned fd
            // is owned by this process and we transfer it into a File below.
            let fd = unsafe { libc::memfd_create(c"rockbox-env".as_ptr(), libc::MFD_CLOEXEC) };
            if fd < 0 {
                return Err(std::io::Error::last_os_error());
            }
            // SAFETY: fd just created; ownership transferred to File which
            // closes it on drop.
            let mut file = unsafe { File::from_raw_fd(fd) };
            use std::io::Write;
            file.write_all(&data)?;
            // SAFETY: file is alive; mmap borrows the fd until dropped.
            let mmap = unsafe { MmapOptions::new().map(&file)? };
            Ok(Self { mmap, _owner: file })
        }
        pub(super) fn as_slice(&self) -> &[u8] {
            &self.mmap
        }
    }

    impl std::fmt::Debug for Backing {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("Backing")
                .field("len", &self.mmap.len())
                .finish()
        }
    }
}

#[cfg(not(target_os = "linux"))]
mod backing {
    pub(super) struct Backing {
        data: Vec<u8>,
    }
    impl Backing {
        pub(super) fn from_vec(data: Vec<u8>) -> std::io::Result<Self> {
            Ok(Self { data })
        }
        pub(super) fn as_slice(&self) -> &[u8] {
            &self.data
        }
    }
    impl std::fmt::Debug for Backing {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("Backing")
                .field("len", &self.data.len())
                .finish()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_blob() {
        let snap = EnvSnapshot {
            vars: vec![
                ("PATH".into(), "/nix/store/abc/bin".into()),
                ("LANG".into(), "C.UTF-8".into()),
            ],
        };
        let blob = snap.to_blob();
        let parsed = EnvSnapshot::parse(&blob);
        assert_eq!(parsed.vars, snap.vars);
    }

    #[test]
    fn cache_hit_and_evict() {
        let c = EnvCache::new(2);
        let k1 = EnvKey {
            digest: Sha256Digest::from_bytes(b"a"),
        };
        let k2 = EnvKey {
            digest: Sha256Digest::from_bytes(b"b"),
        };
        let k3 = EnvKey {
            digest: Sha256Digest::from_bytes(b"c"),
        };
        c.insert(k1.clone(), b"PATH=1".to_vec()).unwrap();
        c.insert(k2.clone(), b"PATH=2".to_vec()).unwrap();
        assert!(c.get(&k1).is_some());
        c.insert(k3.clone(), b"PATH=3".to_vec()).unwrap();
        // k2 (oldest after touch on k1) should be evicted
        assert!(c.get(&k2).is_none());
        assert!(c.get(&k1).is_some());
        assert!(c.get(&k3).is_some());
    }
}
