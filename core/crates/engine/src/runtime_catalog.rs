//! Pre-baked Nix runtimes (ARCH-12). The catalog maps a logical name like
//! `"python-ml"` to:
//!
//! - The path of its frozen `flake.lock`
//! - The name of the interpreter / compiler executable
//! - Default capabilities + env vars
//!
//! Engine refuses to run any `runtime` not in the catalog. New runtimes are
//! added by deployment-side flake builds — no user input.
//!
//! Binary paths are resolved at runtime via `$PATH`. Production deploys
//! prewarm the Nix flakes, which causes `$PATH` to point at `/nix/store/.../bin`
//! entries — the same lookup picks them up. Dev environments where the
//! runtime is just installed system-wide (e.g. `apt install python3`) also
//! resolve via `$PATH`. **No dev/prod code divergence**: same lookup, same
//! code path, the environment decides which binary wins.

use protocol::{Capability, Language};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::OnceLock;

#[derive(Debug, Clone)]
pub struct RuntimeEntry {
    pub name: &'static str,
    pub language: Language,
    pub flake_lock: PathBuf,
    /// Name of the interpreter or compiler executable (e.g. `"python3"`,
    /// `"node"`). Resolved to an absolute path at request time via
    /// [`RuntimeEntry::resolve_bin`].
    pub executable: &'static str,
    /// Capabilities implicitly granted by this runtime.
    pub baseline_caps: &'static [Capability],
    /// Default env vars (overlaid by the user's `settings.env` after
    /// SEC-18/28 filtering).
    pub baseline_env: &'static [(&'static str, &'static str)],
}

impl RuntimeEntry {
    /// Resolve [`Self::executable`] to a canonical `/nix/store/...` path.
    ///
    /// Walks `$PATH`, finds the first directory containing `executable`,
    /// then canonicalizes the result through any symlinks (e.g.
    /// `/root/.nix-profile/bin/python3 → /nix/store/<hash>-python3-3.13/bin/python3`).
    ///
    /// We canonicalize so that the sandbox only needs to bind-mount
    /// `/nix/store` — the symlink path itself wouldn't resolve inside the
    /// mount namespace.
    pub fn resolve_bin(&self) -> std::io::Result<PathBuf> {
        let exe = self.executable;
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
}

pub fn catalog() -> &'static BTreeMap<&'static str, RuntimeEntry> {
    static C: OnceLock<BTreeMap<&'static str, RuntimeEntry>> = OnceLock::new();
    C.get_or_init(build_catalog)
}

pub fn lookup(name: &str) -> Option<&'static RuntimeEntry> {
    catalog().get(name)
}

pub fn default_for(language: Language) -> &'static RuntimeEntry {
    let name = match language {
        Language::Python => "python-base",
        Language::Typescript => "ts-modern",
        Language::Go => "go-std",
        Language::Rust => "rust-tokio",
        Language::Cpp => "cpp-modern",
    };
    catalog()
        .get(name)
        .expect("default runtime must exist in catalog")
}

fn build_catalog() -> BTreeMap<&'static str, RuntimeEntry> {
    let mut m = BTreeMap::new();
    for entry in [
        RuntimeEntry {
            name: "python-base",
            language: Language::Python,
            flake_lock: PathBuf::from("/etc/sandbox/flakes/python-base.lock"),
            executable: "python3",
            baseline_caps: &[Capability::Concurrency],
            baseline_env: &[
                ("PYTHONDONTWRITEBYTECODE", "1"),
                ("PYTHONUNBUFFERED", "1"),
                ("LANG", "C.UTF-8"),
            ],
        },
        RuntimeEntry {
            name: "python-ml",
            language: Language::Python,
            flake_lock: PathBuf::from("/etc/sandbox/flakes/python-ml.lock"),
            executable: "python3",
            baseline_caps: &[Capability::Concurrency, Capability::LargeFs],
            baseline_env: &[
                ("PYTHONDONTWRITEBYTECODE", "1"),
                ("PYTHONUNBUFFERED", "1"),
                ("LANG", "C.UTF-8"),
                ("MPLBACKEND", "Agg"),
                ("OMP_NUM_THREADS", "4"),
                ("MKL_NUM_THREADS", "4"),
            ],
        },
        RuntimeEntry {
            name: "python-web",
            language: Language::Python,
            flake_lock: PathBuf::from("/etc/sandbox/flakes/python-web.lock"),
            executable: "python3",
            baseline_caps: &[Capability::Concurrency],
            baseline_env: &[
                ("PYTHONDONTWRITEBYTECODE", "1"),
                ("PYTHONUNBUFFERED", "1"),
                ("LANG", "C.UTF-8"),
            ],
        },
        RuntimeEntry {
            name: "ts-modern",
            language: Language::Typescript,
            flake_lock: PathBuf::from("/etc/sandbox/flakes/ts-modern.lock"),
            executable: "node",
            baseline_caps: &[Capability::Concurrency],
            baseline_env: &[("NODE_NO_WARNINGS", "1"), ("LANG", "C.UTF-8")],
        },
        RuntimeEntry {
            name: "go-std",
            language: Language::Go,
            flake_lock: PathBuf::from("/etc/sandbox/flakes/go-std.lock"),
            executable: "go",
            baseline_caps: &[Capability::Concurrency],
            baseline_env: &[
                ("GOCACHE", "/tmp/go-cache"),
                ("GOFLAGS", "-mod=mod"),
                ("LANG", "C.UTF-8"),
            ],
        },
        RuntimeEntry {
            name: "rust-tokio",
            language: Language::Rust,
            flake_lock: PathBuf::from("/etc/sandbox/flakes/rust-tokio.lock"),
            executable: "rustc",
            baseline_caps: &[Capability::Concurrency],
            baseline_env: &[("CARGO_HOME", "/tmp/cargo"), ("LANG", "C.UTF-8")],
        },
        RuntimeEntry {
            name: "cpp-modern",
            language: Language::Cpp,
            flake_lock: PathBuf::from("/etc/sandbox/flakes/cpp-modern.lock"),
            executable: "g++",
            baseline_caps: &[Capability::Concurrency],
            baseline_env: &[("LANG", "C.UTF-8")],
        },
    ] {
        m.insert(entry.name, entry);
    }
    m
}
