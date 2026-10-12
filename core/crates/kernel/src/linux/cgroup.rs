//! Cgroup v2 + `cgroup.kill` (FIX PERF-10).
//!
//! We use direct sysfs writes instead of `cgroups-rs` because v2 is simple
//! enough that adding the dependency surface isn't worth it, and we want
//! atomic teardown via `cgroup.kill` (kernel 5.14+) which the crate didn't
//! expose on older versions.
//!
//! Interface files are opened once at [`Cgroup::create`] and reused via
//! `pwrite`/`pread` — a pooled cgroup's hot path (configure + peaks) is
//! then 5 preads/pwrites with no open/close chains.

use crate::error::{SandboxError, SandboxResult};
use crate::spec::ResourceLimits;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::path::{Path, PathBuf};

/// Parent of every sandbox cgroup. Sandboxes used to sit directly under the
/// v2 root, where each one was a sibling of the orchestrator's own cgroup
/// (the container / service slice holding BEAM + engines) at equal weight:
/// 24 busy sandboxes starved the request path 24:1. Under one parent the
/// whole sandbox set is a single sibling, so the orchestrator keeps half the
/// machine under saturation and per-mode `cpu.weight` applies among sandboxes.
pub const CGROUP_ROOT: &str = "/sys/fs/cgroup/rockbox";

/// Create the parent once per process and delegate the controllers the
/// children need. Idempotent across engines sharing the host.
fn ensure_root() -> SandboxResult<()> {
    static ONCE: std::sync::OnceLock<Result<(), String>> = std::sync::OnceLock::new();
    ONCE.get_or_init(|| {
        let root = Path::new(CGROUP_ROOT);
        if let Err(e) = std::fs::create_dir(root)
            && e.kind() != std::io::ErrorKind::AlreadyExists
        {
            return Err(format!("create {}: {e}", root.display()));
        }
        std::fs::write(root.join("cgroup.subtree_control"), "+cpu +memory +pids")
            .map_err(|e| format!("subtree_control: {e}"))
    })
    .clone()
    .map_err(|msg| io_cg("ensure_root", std::io::Error::other(msg)))
}

#[derive(Debug, Clone, Copy)]
pub struct CgroupConfig {
    pub limits: ResourceLimits,
}

#[derive(Debug)]
pub struct Cgroup {
    path: PathBuf,
    dirfd: OwnedFd,
    memory_max: OwnedFd,
    pids_max: OwnedFd,
    cpu_max: OwnedFd,
    cpu_weight: OwnedFd,
    /// `None` when the kernel has no swap accounting for this hierarchy.
    memory_swap_max: Option<OwnedFd>,
    events: OwnedFd,
    memory_peak: OwnedFd,
    pids_peak: OwnedFd,
    /// Set once `cgroup.kill` has been written. The kernel keeps the
    /// CGRP_KILL flag on a killed cgroup, so `clone3(CLONE_INTO_CGROUP)`
    /// into it SIGKILLs the child at birth (fork-bomb defence); such a
    /// cgroup must be removed, never recycled.
    killed: std::sync::atomic::AtomicBool,
}

impl Cgroup {
    /// Create a fresh cgroup directory under [`CGROUP_ROOT`]. Returns an
    /// `O_PATH | O_DIRECTORY` fd suitable for `clone3 + CLONE_INTO_CGROUP`.
    pub fn create(name: &str, cfg: CgroupConfig) -> SandboxResult<Self> {
        ensure_root()?;
        let path = Path::new(CGROUP_ROOT).join(name);
        if path.exists() {
            // Leftover from an earlier engine generation. Reuse is unsafe:
            // a previous `cgroup.kill` left CGRP_KILL set, and
            // CLONE_INTO_CGROUP into it kills the child at birth. Remove and
            // recreate so the pool always starts clean.
            let _ = std::fs::write(path.join("cgroup.kill"), "1");
            for _ in 0..200 {
                match std::fs::remove_dir(&path) {
                    Ok(()) => break,
                    Err(_) => std::thread::sleep(std::time::Duration::from_millis(5)),
                }
            }
        }
        std::fs::create_dir_all(&path).map_err(|e| io_cg("create_dir", e))?;
        let dirfd = open_path_dir(&path)?;
        let cg = Self {
            memory_max: open_file(&path, "memory.max", libc::O_WRONLY)?,
            pids_max: open_file(&path, "pids.max", libc::O_WRONLY)?,
            cpu_max: open_file(&path, "cpu.max", libc::O_WRONLY)?,
            cpu_weight: open_file(&path, "cpu.weight", libc::O_WRONLY)?,
            memory_swap_max: open_file(&path, "memory.swap.max", libc::O_WRONLY).ok(),
            events: open_file(&path, "cgroup.events", libc::O_RDONLY)?,
            memory_peak: open_file(&path, "memory.peak", libc::O_RDWR)?,
            pids_peak: open_file(&path, "pids.peak", libc::O_RDWR)?,
            path,
            dirfd,
            killed: std::sync::atomic::AtomicBool::new(false),
        };
        cg.configure(cfg)?;
        Ok(cg)
    }

    /// (Re)apply resource limits. Called on fresh create and on pool reuse —
    /// reuse therefore costs 4 tiny `pwrite`s instead of the full
    /// mkdir + write + open chain.
    pub fn configure(&self, cfg: CgroupConfig) -> SandboxResult<()> {
        fd_write(
            &self.memory_max,
            cfg.limits.memory_bytes.to_string().as_bytes(),
        )?;
        // No swap: with `memory.swap.max = max` a sandbox at its limit pages
        // out instead of being OOM-killed, so `memory_mb` was never a hard
        // cap on hosts with swap (a 400 MB allocation "succeeded" under 128).
        if let Some(fd) = &self.memory_swap_max {
            fd_write(fd, b"0")?;
        }
        fd_write(&self.pids_max, cfg.limits.pids_max.to_string().as_bytes())?;
        fd_write(
            &self.cpu_max,
            format!("{} {}", cfg.limits.cpu_quota_us, cfg.limits.cpu_period_us).as_bytes(),
        )?;
        fd_write(
            &self.cpu_weight,
            cfg.limits
                .cpu_weight
                .clamp(1, 10_000)
                .to_string()
                .as_bytes(),
        )?;
        Ok(())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn fd(&self) -> RawFd {
        self.dirfd.as_raw_fd()
    }

    /// Whether any task is currently inside this cgroup (reads
    /// `cgroup.events`). Used before recycling a cgroup into the pool.
    pub fn is_populated(&self) -> SandboxResult<bool> {
        Ok(fd_read(&self.events)?.contains("populated 1"))
    }

    /// Move an existing process into this cgroup. Used when CLONE_INTO_CGROUP
    /// isn't available (nested container setups). Equivalent to
    /// `echo $pid > cgroup.procs`.
    pub fn add_pid(&self, pid: i32) -> SandboxResult<()> {
        let p = self.path.join("cgroup.procs");
        std::fs::write(&p, pid.to_string()).map_err(|e| SandboxError::Cgroup {
            op: "cgroup.procs",
            source: e,
        })
    }

    /// Atomic SIGKILL of every task in the cgroup (FIX PERF-10). Marks the
    /// cgroup as killed — the kernel leaves CGRP_KILL set, so recycling it
    /// would SIGKILL any future CLONE_INTO_CGROUP child at birth.
    pub fn kill_all(&self) -> SandboxResult<()> {
        let p = self.path.join("cgroup.kill");
        std::fs::write(&p, "1").map_err(|e| SandboxError::Cgroup {
            op: "cgroup.kill",
            source: e,
        })?;
        self.killed
            .store(true, std::sync::atomic::Ordering::Relaxed);
        Ok(())
    }

    /// Whether `cgroup.kill` was written to this cgroup. Killed cgroups are
    /// unsafe to recycle for CLONE_INTO_CGROUP and must be removed instead.
    pub fn was_killed(&self) -> bool {
        self.killed.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Change only the CPU share (a warm RL worker boots at the batch weight
    /// and is promoted to the interactive weight when an episode claims it).
    pub fn set_cpu_weight(&self, weight: u32) -> SandboxResult<()> {
        fd_write(
            &self.cpu_weight,
            weight.clamp(1, 10_000).to_string().as_bytes(),
        )
    }

    /// Whether the kernel OOM-killed a task in this cgroup (`memory.events`).
    pub fn oom_killed(&self) -> bool {
        std::fs::read_to_string(self.path.join("memory.events"))
            .map(|s| {
                s.lines()
                    .any(|l| l.starts_with("oom_kill ") && l.split_whitespace().nth(1) != Some("0"))
            })
            .unwrap_or(false)
    }

    /// Reset peak counters so the cgroup can be re-used from the pool.
    pub fn reset_peaks(&self) -> SandboxResult<()> {
        // `memory.peak` and `pids.peak` accept "0" to reset on kernel >= 6.0.
        let _ = fd_write(&self.memory_peak, b"0");
        let _ = fd_write(&self.pids_peak, b"0");
        Ok(())
    }

    pub fn current_memory_peak(&self) -> SandboxResult<u64> {
        let s = fd_read(&self.memory_peak)?;
        s.trim().parse().map_err(|_| parse_cg("memory.peak"))
    }

    pub fn wait_empty(&self) -> SandboxResult<()> {
        // Poll `cgroup.events` for `populated 0` (via the cached fd). A
        // SIGKILLed worker is gone within ~100µs; the old fixed 5 ms sleep
        // made every episode close cost 6 ms. Back off 50µs → 5 ms, ~1 s cap.
        let mut nap = std::time::Duration::from_micros(50);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
        while std::time::Instant::now() < deadline {
            if !self.is_populated()? {
                return Ok(());
            }
            std::thread::sleep(nap);
            nap = (nap * 2).min(std::time::Duration::from_millis(5));
        }
        Err(SandboxError::Cgroup {
            op: "wait_empty",
            source: std::io::Error::other("cgroup did not drain"),
        })
    }

    pub fn remove(&self) -> SandboxResult<()> {
        std::fs::remove_dir(&self.path).map_err(|e| io_cg("rmdir", e))
    }
}

fn open_file(root: &Path, file: &str, flags: i32) -> SandboxResult<OwnedFd> {
    use std::os::unix::fs::OpenOptionsExt;
    let f = std::fs::OpenOptions::new()
        .read(flags & libc::O_WRONLY == 0 || flags & libc::O_RDWR != 0)
        .write(flags & (libc::O_WRONLY | libc::O_RDWR) != 0)
        .custom_flags(flags | libc::O_CLOEXEC)
        .open(root.join(file))
        .map_err(|e| io_cg(leak_op(file), e))?;
    Ok(f.into())
}

fn open_path_dir(path: &Path) -> SandboxResult<OwnedFd> {
    use std::os::unix::fs::OpenOptionsExt;
    let f = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC)
        .open(path)
        .map_err(|e| io_cg("open dirfd", e))?;
    Ok(f.into())
}

fn fd_write(fd: &OwnedFd, value: &[u8]) -> SandboxResult<()> {
    // SAFETY: pwrite is stable; fd is a valid open file.
    let n = unsafe { libc::pwrite(fd.as_raw_fd(), value.as_ptr() as *const _, value.len(), 0) };
    if n < 0 {
        return Err(SandboxError::Cgroup {
            op: "pwrite",
            source: std::io::Error::last_os_error(),
        });
    }
    Ok(())
}

fn fd_read(fd: &OwnedFd) -> SandboxResult<String> {
    let mut buf = [0u8; 4096];
    // SAFETY: pread is stable; buf is a valid byte slice.
    let n = unsafe { libc::pread(fd.as_raw_fd(), buf.as_mut_ptr() as *mut _, buf.len(), 0) };
    if n < 0 {
        return Err(SandboxError::Cgroup {
            op: "pread",
            source: std::io::Error::last_os_error(),
        });
    }
    Ok(String::from_utf8_lossy(&buf[..n as usize]).into_owned())
}

fn io_cg(op: &'static str, source: std::io::Error) -> SandboxError {
    SandboxError::Cgroup { op, source }
}

fn parse_cg(op: &'static str) -> SandboxError {
    SandboxError::Cgroup {
        op,
        source: std::io::Error::other("parse u64"),
    }
}

/// `op` lives forever — the strings here are interned via `Box::leak`. We
/// only ever call this with short cgroup-file names, so the leak is bounded.
fn leak_op(name: &str) -> &'static str {
    Box::leak(name.to_string().into_boxed_str())
}
