//! Top-level orchestrator wiring all 10 layers together.
//!
//! The launcher owns the spawn protocol: parent → child sync via the
//! [`super::userns`] pipe, in-child mount/seccomp/cap sequence, and parent
//! drain loop via [`super::drain`].
//!
//! Most layers are stand-alone testable; only [`SandboxLauncher::launch`]
//! requires elevated privileges (CAP_SYS_ADMIN to create namespaces) and
//! therefore is exercised in integration tests rather than unit tests.
use super::{apparmor, caps, cgroup, clone3, drain, mounts, seccomp, userns};
use crate::error::{SandboxError, SandboxResult};
use crate::spec::ChildSpec;
use std::os::fd::OwnedFd;
use std::time::Duration;
use tracing::{error, instrument};

#[derive(Debug)]
pub struct ChildHandle {
    pub pid: i32,
    pub pidfd: OwnedFd,
    pub cgroup: cgroup::Cgroup,
    pub stdout: OwnedFd,
    pub stderr: OwnedFd,
    pub wall: Duration,
}

impl ChildHandle {
    pub const fn pid(&self) -> i32 {
        self.pid
    }
}

pub use super::drain::ChildExit;

#[derive(Debug, Default)]
pub struct SandboxLauncher {
    seccomp: seccomp::SeccompResolver,
}

impl SandboxLauncher {
    pub fn new() -> SandboxResult<Self> {
        Ok(Self {
            seccomp: seccomp::SeccompResolver::new(),
        })
    }

    pub fn seccomp_resolver(&self) -> &seccomp::SeccompResolver {
        &self.seccomp
    }

    /// Spawn a sandboxed child. Returns a [`ChildHandle`] for the parent
    /// to drain via [`drain::Drainer`].
    #[instrument(skip(self, spec), fields(req = %spec.request_id, lang = ?spec.language))]
    pub fn launch(&self, spec: &ChildSpec) -> SandboxResult<ChildHandle> {
        let cgroup_cfg = cgroup::CgroupConfig {
            limits: spec.limits,
        };
        let cg = cgroup::Cgroup::create(&format!("rockbox-{}", spec.request_id), cgroup_cfg)?;

        // Pre-create the pipes BEFORE clone (PERF — avoids post-fork allocation).
        let (stdout_r, stdout_w) = make_pipe()?;
        let (stderr_r, stderr_w) = make_pipe()?;
        let sync = userns::make_sync_pipe()?;

        let bpf = self.seccomp.compile(spec.seccomp_profile)?;

        // Compile owned data the child needs into a copy; the parent will
        // continue executing post-clone but we need everything pre-fork-safe.
        // CLONE_INTO_CGROUP requires the target cgroup to be unpopulated and
        // for the calling task to have write access; nested container setups
        // (Docker on macOS, k8s with restricted cgroup v2) often violate one
        // or the other. We do the safer two-step: clone first, then write
        // child_pid into cgroup.procs from the parent.
        let cloned = clone3::clone3(clone3::CloneRequest {
            namespaces: clone3::CloneRequest::full_isolation(),
            cgroup_fd: None,
            exit_signal: libc::SIGCHLD as u64,
        })?;

        if cloned.child_pid == 0 {
            // CHILD: block until parent writes uid_map, then set up everything.
            let _ = drop(sync.write_end);
            if let Err(e) = userns::child_wait_for_release(&sync.read_end) {
                eprintln!("child sync: {e}");
                std::process::exit(101);
            }
            drop(sync.read_end);
            drop(stdout_r);
            drop(stderr_r);

            // Reset fsuid/fsgid to in-NS root. The clone3 inherits the
            // parent's kuid which only becomes "in-NS root" once uid_map is
            // in place AND we re-set the IDs. Without this, mkdir/chown on
            // freshly-created inodes fails with EOVERFLOW.
            // SAFETY: setresuid/setresgid are standard syscalls; we have
            // CAP_SETUID in the new user namespace.
            unsafe {
                libc::setresgid(0, 0, 0);
                libc::setresuid(0, 0, 0);
            }

            if let Err(e) = run_child(spec, stdout_w, stderr_w, &bpf) {
                eprintln!("child setup: {e}");
                std::process::exit(102);
            }
            unreachable!("execve should have replaced the process");
        }

        // PARENT.
        let userns::SyncPipe {
            read_end,
            write_end,
        } = sync;
        drop(read_end);
        drop(stdout_w);
        drop(stderr_w);

        userns::write_id_maps(cloned.child_pid).map_err(|e| {
            error!(?e, "uid_map");
            e
        })?;

        // Place the child in our cgroup. Best-effort: nested-container hosts
        // may not give us write access to cgroup.procs, in which case the
        // sandbox still runs (with RLIMIT-only resource bounds) and we log.
        if let Err(e) = cg.add_pid(cloned.child_pid) {
            tracing::warn!(?e, child = cloned.child_pid, "cgroup_add_pid_failed");
        }

        // EPIPE here means the child already raced past the read (e.g. saw EOF
        // because we exited early on an earlier error). Log + continue —
        // there's no one left to signal anyway.
        if let Err(e) = userns::release_child(&write_end) {
            tracing::warn!(?e, "release_child");
        }
        drop(write_end);

        let pidfd = cloned
            .pidfd
            .ok_or_else(|| crate::error::SandboxError::Clone("parent missing pidfd".into()))?;
        Ok(ChildHandle {
            pid: cloned.child_pid,
            pidfd,
            cgroup: cg,
            stdout: stdout_r,
            stderr: stderr_r,
            wall: spec.wall_timeout,
        })
    }

    pub fn make_drainer(
        &self,
        h: ChildHandle,
        output_cap: u64,
    ) -> SandboxResult<(cgroup::Cgroup, drain::Drainer)> {
        let drainer = drain::Drainer::new(h.stdout, h.stderr, h.pidfd, h.wall, output_cap)?;
        Ok((h.cgroup, drainer))
    }
}

fn run_child(
    spec: &ChildSpec,
    stdout_w: OwnedFd,
    stderr_w: OwnedFd,
    bpf: &seccomp::BpfProfile,
) -> SandboxResult<()> {
    // 1. Redirect stdout/stderr to pipes.
    redirect_to_pipe(stdout_w, libc::STDOUT_FILENO)?;
    redirect_to_pipe(stderr_w, libc::STDERR_FILENO)?;

    // 2. Mount namespace setup (Layer 3).
    let mut plan = mounts::MountPlan::new("/tmp/rockbox-newroot");
    for m in &spec.mounts {
        plan.add(m.clone());
    }
    plan.apply()?;

    // 3. AppArmor (Layer 0/8) — staged for next exec.
    if apparmor::is_available() {
        apparmor::change_onexec(&spec.apparmor_profile)?;
    }

    // 4. NO_NEW_PRIVS (Layer 7) BEFORE seccomp (FIX SEC-14).
    caps::set_no_new_privs()?;
    if spec.layers.enforce_w_xor_x {
        let _ = caps::enable_mdwe();
    }

    // 5. Seccomp (Layer 3 in §6). Stateless apply — the BPF blob was compiled
    // once at engine boot and is now just being reloaded into this task.
    seccomp::SeccompResolver::apply(bpf)?;

    // 6. Drop caps + rlimits (Layers 5 & 6).
    caps::drop_all_capabilities()?;
    caps::apply_rlimits(&spec.limits)?;

    // 7. execve via fexecve (verified fd) or pinned interpreter path.
    let argv: Vec<std::ffi::CString> = spec
        .argv
        .iter()
        .map(|s| std::ffi::CString::new(s.as_bytes()).expect("argv must be UTF-8 sans NUL"))
        .collect();
    let env: Vec<std::ffi::CString> = spec
        .env
        .iter()
        .map(|(k, v)| {
            std::ffi::CString::new(format!("{k}={v}")).expect("env must be UTF-8 sans NUL")
        })
        .collect();
    let argv_ptrs: Vec<*const libc::c_char> = argv
        .iter()
        .map(|c| c.as_ptr())
        .chain(std::iter::once(std::ptr::null()))
        .collect();
    let env_ptrs: Vec<*const libc::c_char> = env
        .iter()
        .map(|c| c.as_ptr())
        .chain(std::iter::once(std::ptr::null()))
        .collect();

    if let Some(fd_path) = &spec.binary_fd_path {
        let exe = std::ffi::CString::new(fd_path.as_bytes()).expect("fd path");
        // SAFETY: execve is a stable syscall; pointers above are NUL-terminated.
        unsafe {
            libc::execve(exe.as_ptr(), argv_ptrs.as_ptr(), env_ptrs.as_ptr());
        }
    } else {
        // SAFETY: same as above.
        unsafe {
            libc::execve(argv[0].as_ptr(), argv_ptrs.as_ptr(), env_ptrs.as_ptr());
        }
    }
    // If we got here, execve failed.
    Err(SandboxError::Internal(format!(
        "execve: {}",
        std::io::Error::last_os_error()
    )))
}

fn make_pipe() -> SandboxResult<(OwnedFd, OwnedFd)> {
    use nix::fcntl::OFlag;
    use nix::unistd::pipe2;
    let (r, w) = pipe2(OFlag::O_CLOEXEC | OFlag::O_NONBLOCK)
        .map_err(|e| SandboxError::Internal(format!("pipe2: {e}")))?;
    Ok((r, w))
}

fn redirect_to_pipe(pipe: OwnedFd, target_fd: libc::c_int) -> SandboxResult<()> {
    use std::os::fd::AsRawFd;
    let src = pipe.as_raw_fd();
    // SAFETY: dup2 is stable.
    let rc = unsafe { libc::dup2(src, target_fd) };
    if rc < 0 {
        return Err(SandboxError::Internal(format!(
            "dup2: {}",
            std::io::Error::last_os_error()
        )));
    }
    // Original pipe fd intentionally dropped here so only the duped fd survives.
    drop(pipe);
    Ok(())
}
