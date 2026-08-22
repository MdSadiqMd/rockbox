//! Builds the [`MountKind`] list for the sandbox.
//!
//! The order of returned mounts matters — the mount-namespace setup walks the
//! list in sequence and creates parent directories on demand.
//!
//! What gets bound:
//!
//! - `/nix/store`     read-only — required, every runtime lives here
//! - pinned files in `/etc` (passwd, group, resolv.conf, hosts, CA bundle)
//! - `/proc`, `/tmp`, `/dev-min`
//! - `/sandbox` (user code)
//! - `/session` / `/episode` if the mode requires them
//!
//! Identical in dev and production — both rely on Nix-installed runtimes
//! that are self-contained within `/nix/store`. The dev container's
//! `Dockerfile.dev` runs `nix profile install` for the catalog so this
//! works without any "if dev" branch.

use kernel::spec::MountKind;
use protocol::{Mode, Settings};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

const NIX_STORE: &str = "/nix/store";
const DEV_MIN: &str = "/var/lib/sandbox/dev-min";
const SESSIONS_ROOT: &str = "/var/lib/sandbox/sessions";
const EPISODES_ROOT: &str = "/var/lib/sandbox/episodes";

/// Pinned files from /etc. We deliberately avoid binding all of /etc so
/// host secrets (shadow, sudoers, ssh keys) never leak into the sandbox.
const ETC_FILES: &[&str] = &[
    "/etc/passwd",
    "/etc/group",
    "/etc/nsswitch.conf",
    "/etc/resolv.conf",
    "/etc/hosts",
    "/etc/ssl/certs/ca-certificates.crt",
];

/// Host filesystem facts that are static for the engine's lifetime. The
/// resolver previously re-statted `/nix/store`, `/var/lib/sandbox/dev-min`
/// and each `/etc` file on every request (~10 syscalls); they are evaluated
/// once and cached.
struct HostFacts {
    dev_min: bool,
    nix_store: bool,
    etc: Vec<bool>,
}

fn host_facts() -> &'static HostFacts {
    static FACTS: OnceLock<HostFacts> = OnceLock::new();
    FACTS.get_or_init(|| HostFacts {
        dev_min: Path::new(DEV_MIN).is_dir(),
        nix_store: Path::new(NIX_STORE).is_dir(),
        etc: ETC_FILES.iter().map(|f| Path::new(f).exists()).collect(),
    })
}

pub fn resolve(settings: &Settings, work_dir: &PathBuf, bin_path: &Path) -> Vec<MountKind> {
    let facts = host_facts();
    let mut out: Vec<MountKind> = Vec::with_capacity(16);

    out.push(MountKind::Proc {
        target: PathBuf::from("/proc"),
    });
    out.push(MountKind::Tmpfs {
        target: PathBuf::from("/tmp"),
        size_bytes: settings.limits.tmpfs_mb.saturating_mul(1024 * 1024),
        mode: 0o1777,
    });

    if facts.dev_min {
        out.push(MountKind::DevMin {
            src: PathBuf::from(DEV_MIN),
            target: PathBuf::from("/dev"),
        });
    } else {
        // No pre-built /dev-min template on this host — build the equivalent
        // by bind-mounting individual char devices from the host's /dev into
        // an empty tmpfs. (mknod() of char devices is forbidden inside a
        // user namespace, but bind-mount of an existing inode works.)
        out.push(MountKind::Tmpfs {
            target: PathBuf::from("/dev"),
            size_bytes: 4 * 1024 * 1024,
            mode: 0o755,
        });
        for dev in ["null", "zero", "urandom", "random", "tty", "full"] {
            let host = PathBuf::from(format!("/dev/{dev}"));
            if host.exists() {
                out.push(MountKind::BindRw {
                    src: host,
                    target: PathBuf::from(format!("/dev/{dev}")),
                });
            }
        }
    }

    // /nix/store: bind read-only so the interpreter (Python, Node, Go
    // toolchain) and its dynamic libs are visible inside the sandbox.
    // Skipped for languages that produce self-contained static binaries
    // (currently Go with CGO_ENABLED=0) — the pre-compiled binary carries
    // everything it needs, shaving off one bind-mount and the mount overhead.
    // For Go yaegi fast path, we need the yaegi binary itself.
    // For trivial shell-script fast path (all langs), we need /bin/sh.
    let needs_nix_store = match settings.language {
        protocol::Language::Go => {
            // yaegi fast path needs its binary; check if it exists and the
            // program is single-file (same heuristic as spec.rs).
            let yaegi_path = Path::new("/usr/local/bin/yaegi");
            if yaegi_path.exists()
                && settings.files.len() == 1
                && !settings.files[0]
                    .content
                    .windows(9)
                    .any(|w| w == b"import \"C\"")
                && !settings.files[0].content.windows(6).any(|w| w == b"unsafe")
            {
                out.push(MountKind::BindRo {
                    src: PathBuf::from("/usr/local/bin/yaegi"),
                    target: PathBuf::from("/usr/local/bin/yaegi"),
                });
            }
            // Trivial shell-script fast path needs /bin/sh + /bin/echo + /bin/cat + libs
            for bin in [
                "/bin/sh",
                "/bin/dash",
                "/bin/bash",
                "/bin/echo",
                "/usr/bin/echo",
                "/bin/cat",
                "/usr/bin/cat",
            ] {
                if Path::new(bin).exists() {
                    out.push(MountKind::BindRo {
                        src: PathBuf::from(bin),
                        target: PathBuf::from(bin),
                    });
                }
            }
            if let Ok(real) = std::fs::read_link("/bin/sh") {
                let real_path = if real.is_absolute() {
                    real
                } else {
                    Path::new("/bin").join(real)
                };
                if real_path.exists() && real_path != Path::new("/bin/sh") {
                    out.push(MountKind::BindRo {
                        src: real_path.clone(),
                        target: real_path,
                    });
                }
            }
            for lib in ["/lib", "/lib64", "/usr/lib", "/usr/lib/aarch64-linux-gnu"] {
                if Path::new(lib).is_dir() {
                    out.push(MountKind::BindRo {
                        src: PathBuf::from(lib),
                        target: PathBuf::from(lib),
                    });
                }
            }
            false
        }
        _ => {
            // Trivial shell-script fast path for Rust/C++ also needs /bin/sh
            for bin in [
                "/bin/sh",
                "/bin/dash",
                "/bin/bash",
                "/bin/echo",
                "/usr/bin/echo",
                "/bin/cat",
                "/usr/bin/cat",
            ] {
                if Path::new(bin).exists() {
                    out.push(MountKind::BindRo {
                        src: PathBuf::from(bin),
                        target: PathBuf::from(bin),
                    });
                }
            }
            if let Ok(real) = std::fs::read_link("/bin/sh") {
                let real_path = if real.is_absolute() {
                    real
                } else {
                    Path::new("/bin").join(real)
                };
                if real_path.exists() && real_path != Path::new("/bin/sh") {
                    out.push(MountKind::BindRo {
                        src: real_path.clone(),
                        target: real_path,
                    });
                }
            }
            for lib in ["/lib", "/lib64", "/usr/lib", "/usr/lib/aarch64-linux-gnu"] {
                if Path::new(lib).is_dir() {
                    out.push(MountKind::BindRo {
                        src: PathBuf::from(lib),
                        target: PathBuf::from(lib),
                    });
                }
            }
            true
        }
    };
    if needs_nix_store && facts.nix_store {
        out.push(MountKind::BindRo {
            src: PathBuf::from(NIX_STORE),
            target: PathBuf::from(NIX_STORE),
        });
    }

    // Sanity: for interpreted/JIT languages the binary MUST live in the store.
    debug_assert!(
        !needs_nix_store || bin_path.starts_with(NIX_STORE) || !facts.nix_store,
        "resolved bin {bin_path:?} is not under {NIX_STORE}"
    );

    for (f, exists) in ETC_FILES.iter().zip(facts.etc.iter()) {
        if *exists {
            out.push(MountKind::BindRo {
                src: PathBuf::from(*f),
                target: PathBuf::from(*f),
            });
        }
    }

    out.push(MountKind::BindRo {
        src: work_dir.clone(),
        target: PathBuf::from("/sandbox"),
    });

    if matches!(settings.mode, Mode::Session)
        && let Some(sid) = &settings.session_id
    {
        let src = PathBuf::from(SESSIONS_ROOT).join(sid);
        if src.is_dir() {
            out.push(MountKind::BindRw {
                src,
                target: PathBuf::from("/session"),
            });
        }
    }
    if matches!(settings.mode, Mode::RlStep | Mode::RlEpisode) {
        let src = PathBuf::from(EPISODES_ROOT).join(&settings.request_id);
        if src.is_dir() {
            out.push(MountKind::BindRw {
                src,
                target: PathBuf::from("/episode"),
            });
        }
    }

    for em in &settings.filesystem.mounts {
        let Some(src) = resolve_source(&em.source) else {
            continue;
        };
        match em.mode {
            protocol::settings::MountMode::Ro => out.push(MountKind::BindRo {
                src,
                target: PathBuf::from(&em.target),
            }),
            protocol::settings::MountMode::Rw => out.push(MountKind::BindRw {
                src,
                target: PathBuf::from(&em.target),
            }),
        }
    }

    out
}

fn resolve_source(uri: &str) -> Option<PathBuf> {
    if let Some(sid) = uri.strip_prefix("session:") {
        Some(PathBuf::from(SESSIONS_ROOT).join(sid))
    } else if let Some(eid) = uri.strip_prefix("episode:") {
        Some(PathBuf::from(EPISODES_ROOT).join(eid))
    } else if let Some(vid) = uri.strip_prefix("volume:") {
        Some(PathBuf::from("/var/lib/sandbox/volumes").join(vid))
    } else {
        None
    }
}
