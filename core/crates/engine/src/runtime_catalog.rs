//! Pre-baked Nix runtimes (ARCH-12) + user-defined custom environments.
//!
//! The catalog maps a logical name like `"python-ml"` to:
//!
//! - The path of its frozen `flake.lock`
//! - The name of the interpreter / compiler executable
//! - Default capabilities + env vars
//!
//! Engine refuses to run any `runtime` not resolvable through either the
//! built-in catalog or the custom-environment registry.
//!
//! **Custom environments**: the orchestrator builds a user-requested Nix
//! profile out-of-band and drops a JSON descriptor into
//! `$ROCKBOX_CUSTOM_RUNTIMES_DIR` (default `/etc/sandbox/custom-runtimes`).
//! The engine picks descriptors up lazily per lookup (mtime-memoized stat),
//! so no wire-protocol change or engine restart is needed and every engine
//! process on the host sees new environments immediately.
//!
//! Binary resolution: entries carry either an explicit canonical binary
//! path (custom envs — written by the orchestrator after `nix build`) or an
//! executable name resolved via `$PATH` (built-ins). Both end canonicalized
//! under `/nix/store` so the sandbox only bind-mounts the store.
//!
//! Dev environments where the runtime is just installed system-wide also
//! resolve via `$PATH`. **No dev/prod code divergence**: same lookup, same
//! code path, the environment decides which binary wins.

use protocol::{Capability, Language};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock, RwLock};
use std::time::SystemTime;

#[derive(Debug)]
pub struct RuntimeEntry {
    pub name: String,
    pub language: Language,
    pub flake_lock: PathBuf,
    /// Name of the interpreter / compiler executable (e.g. `"python3"`),
    /// resolved to an absolute path by [`RuntimeEntry::resolve_bin`] unless
    /// [`Self::explicit_bin`] short-circuits the PATH walk.
    pub executable: String,
    /// Canonical store path pinned by the orchestrator (custom envs only).
    pub explicit_bin: Option<PathBuf>,
    pub baseline_caps: Vec<Capability>,
    pub baseline_env: Vec<(String, String)>,
    /// Extra interpreter flags prepended to the child argv (e.g. `-S` to
    /// skip the Python site import when the runtime has no site-packages).
    pub interpreter_flags: Vec<String>,
    bin: OnceLock<std::io::Result<PathBuf>>,
    lock: OnceLock<Vec<u8>>,
}

impl RuntimeEntry {
    fn new(
        name: impl Into<String>,
        language: Language,
        flake_lock: impl Into<PathBuf>,
        executable: impl Into<String>,
        baseline_caps: Vec<Capability>,
        baseline_env: Vec<(&'static str, &'static str)>,
        interpreter_flags: Vec<&'static str>,
    ) -> Self {
        RuntimeEntry {
            name: name.into(),
            language,
            flake_lock: flake_lock.into(),
            executable: executable.into(),
            explicit_bin: None,
            baseline_caps,
            baseline_env: baseline_env
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            interpreter_flags: interpreter_flags.into_iter().map(String::from).collect(),
            bin: OnceLock::new(),
            lock: OnceLock::new(),
        }
    }

    /// Resolve the runtime's interpreter/compiler to a canonical absolute
    /// path. Custom envs pin their binary explicitly; built-ins walk $PATH
    /// then canonicalize through symlinks (e.g. `/root/.nix-profile/bin/
    /// python3 → /nix/store/<hash>-python3-3.13/bin/python3`). Result cached
    /// per entry for the engine's lifetime.
    pub fn resolve_bin(&self) -> Result<&Path, &std::io::Error> {
        self.bin
            .get_or_init(|| self.resolve_bin_uncached())
            .as_ref()
            .map(|p| p.as_path())
    }

    fn resolve_bin_uncached(&self) -> std::io::Result<PathBuf> {
        if let Some(pinned) = &self.explicit_bin {
            return std::fs::canonicalize(pinned);
        }
        let exe = &self.executable;
        let path_var = std::env::var_os("PATH")
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "$PATH is unset"))?;
        for dir in std::env::split_paths(&path_var) {
            let candidate = dir.join(exe);
            if candidate.is_file() {
                // Canonicalize — chase symlinks down to the real store path.
                return std::fs::canonicalize(&candidate);
            }
        }
        Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("{exe} not found on $PATH"),
        ))
    }

    /// Stable identity bytes for cache keys (compile cache, work dirs).
    /// Built-ins use the frozen `flake.lock` content; custom envs use their
    /// descriptor file content (the descriptor pins store paths + digest).
    pub fn flake_lock_bytes(&self) -> &[u8] {
        self.lock
            .get_or_init(|| {
                std::fs::read(&self.flake_lock).unwrap_or_else(|_| self.name.as_bytes().to_vec())
            })
            .as_slice()
    }
}

fn entry(
    name: &'static str,
    language: Language,
    flake_lock: &'static str,
    executable: &'static str,
    baseline_caps: &[Capability],
    baseline_env: &[(&'static str, &'static str)],
) -> RuntimeEntry {
    entry_flags(
        name,
        language,
        flake_lock,
        executable,
        baseline_caps,
        baseline_env,
        &[],
    )
}

fn entry_flags(
    name: &'static str,
    language: Language,
    flake_lock: &'static str,
    executable: &'static str,
    baseline_caps: &[Capability],
    baseline_env: &[(&'static str, &'static str)],
    interpreter_flags: &[&'static str],
) -> RuntimeEntry {
    RuntimeEntry::new(
        name,
        language,
        flake_lock,
        executable,
        baseline_caps.to_vec(),
        baseline_env.to_vec(),
        interpreter_flags.to_vec(),
    )
}

pub type SharedEntry = Arc<RuntimeEntry>;

pub fn catalog() -> &'static BTreeMap<&'static str, SharedEntry> {
    static C: OnceLock<BTreeMap<&'static str, SharedEntry>> = OnceLock::new();
    C.get_or_init(build_catalog)
}

pub fn default_for(language: Language) -> &'static RuntimeEntry {
    let name = match language {
        Language::Python => "python-base",
        // Bun boots in ~0.6ms vs node's ~13ms and strips TS natively; node
        // remains available as the explicit `runtime: "ts-modern"` opt-in.
        Language::Typescript => "ts-bun",
        Language::Go => "go-std",
        Language::Rust => "rust-tokio",
        Language::Cpp => "cpp-modern",
    };
    catalog()
        .get(name)
        .expect("default runtime must exist in catalog")
        .as_ref()
}

/// Resolve a runtime by name: built-in catalog first, then the custom
/// environment registry.
pub fn lookup(name: &str) -> Option<SharedEntry> {
    if let Some(e) = catalog().get(name) {
        return Some(Arc::clone(e));
    }
    custom::lookup(name)
}

/// Registry of user-defined environments, backed by JSON descriptors on
/// disk. Lookups stat the descriptor and reload on mtime change; misses are
/// negative-cached until the directory mtime changes so hot paths never pay
/// repeated readdir costs.
mod custom {
    use super::*;

    pub(super) const PREFIX: &str = "custom-";

    #[derive(Deserialize)]
    struct Descriptor {
        language: Language,
        #[serde(default)]
        executable: Option<String>,
        bin: PathBuf,
        #[serde(default)]
        env: BTreeMap<String, String>,
        #[serde(default)]
        interpreter_flags: Vec<String>,
        lock_hex: Option<String>,
    }

    struct Cached {
        loaded_at: SystemTime,
        entry: Option<SharedEntry>,
    }

    static REGISTRY: RwLock<Option<BTreeMap<String, Cached>>> = RwLock::new(None);

    pub(super) fn dir() -> PathBuf {
        std::env::var_os("ROCKBOX_CUSTOM_RUNTIMES_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/etc/sandbox/custom-runtimes"))
    }

    fn valid_name(name: &str) -> bool {
        name.len() > PREFIX.len()
            && name.len() <= PREFIX.len() + 64
            && name[PREFIX.len()..]
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    }

    pub(super) fn lookup(name: &str) -> Option<SharedEntry> {
        if !valid_name(name) {
            return None;
        }
        let path = dir().join(format!("{name}.json"));
        let mtime = std::fs::metadata(&path).ok()?.modified().ok()?;

        let mut guard = REGISTRY.write().ok()?;
        let map = guard.get_or_insert_with(BTreeMap::new);
        match map.get(name) {
            Some(c) if c.loaded_at == mtime => return c.entry.clone(),
            _ => {}
        }

        let entry = load(&path, name);
        map.insert(
            name.to_string(),
            Cached {
                loaded_at: mtime,
                entry: entry.clone(),
            },
        );
        entry
    }

    fn load(path: &Path, name: &str) -> Option<SharedEntry> {
        let raw = std::fs::read(path).ok()?;
        let d: Descriptor = serde_json::from_slice(&raw).ok()?;
        let entry = RuntimeEntry {
            name: name.to_string(),
            language: d.language,
            flake_lock: path.to_path_buf(),
            executable: d.executable.unwrap_or_else(|| "python3".to_string()),
            explicit_bin: Some(d.bin),
            baseline_caps: vec![Capability::Concurrency],
            baseline_env: {
                let mut v: Vec<(String, String)> = d.env.into_iter().collect();
                v.push(("LANG".to_string(), "C.UTF-8".to_string()));
                v
            },
            interpreter_flags: d.interpreter_flags,
            bin: OnceLock::new(),
            lock: OnceLock::new(),
        };
        // Identity: the descriptor content itself pins the environment.
        entry.lock.get_or_init(|| {
            let mut bytes = raw.clone();
            if let Some(hex) = &d.lock_hex {
                bytes.extend_from_slice(hex.as_bytes());
            }
            bytes
        });
        Some(Arc::new(entry))
    }
}

fn build_catalog() -> BTreeMap<&'static str, SharedEntry> {
    let mut m = BTreeMap::new();
    for e in [
        entry_flags(
            "python-base",
            Language::Python,
            "/etc/sandbox/flakes/python-base.lock",
            "python3",
            &[Capability::Concurrency],
            &[
                ("PYTHONDONTWRITEBYTECODE", "1"),
                ("PYTHONUNBUFFERED", "1"),
                ("LANG", "C.UTF-8"),
            ],
            // `-S` skips the site import (sitecustomize, site-packages
            // scan) — python-base ships no packages, so the only thing it
            // buys is ~4ms of interpreter boot. Runtimes with packages
            // (python-ml/web, custom envs) must keep site so NIX_PYTHONPATH
            // resolves.
            &["-S"],
        ),
        entry(
            "python-ml",
            Language::Python,
            "/etc/sandbox/flakes/python-ml.lock",
            "python3",
            &[Capability::Concurrency, Capability::LargeFs],
            &[
                ("PYTHONDONTWRITEBYTECODE", "1"),
                ("PYTHONUNBUFFERED", "1"),
                ("LANG", "C.UTF-8"),
                ("MPLBACKEND", "Agg"),
                ("OMP_NUM_THREADS", "4"),
                ("MKL_NUM_THREADS", "4"),
            ],
        ),
        entry(
            "python-web",
            Language::Python,
            "/etc/sandbox/flakes/python-web.lock",
            "python3",
            &[Capability::Concurrency],
            &[
                ("PYTHONDONTWRITEBYTECODE", "1"),
                ("PYTHONUNBUFFERED", "1"),
                ("LANG", "C.UTF-8"),
            ],
        ),
        entry(
            "ts-modern",
            Language::Typescript,
            "/etc/sandbox/flakes/ts-modern.lock",
            "node",
            &[Capability::Concurrency],
            &[("NODE_NO_WARNINGS", "1"), ("LANG", "C.UTF-8")],
        ),
        entry(
            "ts-bun",
            Language::Typescript,
            "/etc/sandbox/flakes/ts-bun.lock",
            "bun",
            &[Capability::Concurrency],
            &[
                // Keep bun's own state dirs inside the sandbox tmpfs: the
                // mount plan exposes no writable HOME.
                ("BUN_INSTALL", "/tmp/.bun"),
                ("XDG_CACHE_HOME", "/tmp/.cache"),
                ("LANG", "C.UTF-8"),
            ],
        ),
        entry(
            "go-std",
            Language::Go,
            "/etc/sandbox/flakes/go-std.lock",
            "go",
            &[Capability::Concurrency],
            &[
                ("GOCACHE", "/tmp/go-cache"),
                ("GOFLAGS", "-mod=mod"),
                ("LANG", "C.UTF-8"),
            ],
        ),
        entry(
            "rust-tokio",
            Language::Rust,
            "/etc/sandbox/flakes/rust-tokio.lock",
            "rustc",
            &[Capability::Concurrency],
            &[("CARGO_HOME", "/tmp/cargo"), ("LANG", "C.UTF-8")],
        ),
        entry(
            "cpp-modern",
            Language::Cpp,
            "/etc/sandbox/flakes/cpp-modern.lock",
            "g++",
            &[Capability::Concurrency],
            &[("LANG", "C.UTF-8")],
        ),
    ] {
        m.insert(
            Box::leak(e.name.clone().into_boxed_str()) as &'static str,
            Arc::new(e),
        );
    }
    m
}
