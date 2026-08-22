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

#[derive(Debug)]
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
    /// Extra interpreter flags prepended to the child argv (e.g. `-S` to
    /// skip the Python site import when the runtime has no site-packages).
    pub interpreter_flags: &'static [&'static str],
    /// Cached result of [`RuntimeEntry::resolve_bin`]. The engine process is
    /// long-lived and `$PATH` is stable for its lifetime, so the PATH walk +
    /// canonicalize syscall chain runs exactly once per runtime instead of
    /// on every request.
    bin: OnceLock<std::io::Result<PathBuf>>,
    /// Content of the runtime's frozen `flake.lock` (or the runtime name if
    /// the file is absent on dev hosts). Used as the compiler identity in
    /// [`cache::BinaryKey`] — a changed lock means a changed toolchain.
    lock: OnceLock<Vec<u8>>,
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
    ///
    /// Result is cached for the engine's lifetime; callers may hold the
    /// returned reference indefinitely. Requires `&'static Self` because the
    /// cache lives inside the catalog's `OnceLock` (entries are never
    /// constructed elsewhere).
    pub fn resolve_bin(self: &'static Self) -> Result<&'static PathBuf, &'static std::io::Error> {
        self.bin
            .get_or_init(|| self.resolve_bin_uncached())
            .as_ref()
    }

    fn resolve_bin_uncached(&self) -> std::io::Result<PathBuf> {
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

    /// Stable identity bytes for the pinned toolchain. The frozen
    /// `flake.lock` content when readable, else the runtime name.
    pub fn flake_lock_bytes(self: &'static Self) -> &'static [u8] {
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
    baseline_caps: &'static [Capability],
    baseline_env: &'static [(&'static str, &'static str)],
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
    baseline_caps: &'static [Capability],
    baseline_env: &'static [(&'static str, &'static str)],
    interpreter_flags: &'static [&'static str],
) -> RuntimeEntry {
    RuntimeEntry {
        name,
        language,
        flake_lock: PathBuf::from(flake_lock),
        executable,
        baseline_caps,
        baseline_env,
        interpreter_flags,
        bin: OnceLock::new(),
        lock: OnceLock::new(),
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
            // (python-ml/web) must keep site so NIX_PYTHONPATH resolves.
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
        m.insert(e.name, e);
    }
    m
}
