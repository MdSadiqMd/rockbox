//! Top-level orchestrator wiring all 10 layers together.
//!
//! The launcher owns the spawn protocol. All per-request work that can be
//! amortized is: seccomp BPF is compiled once at boot, cgroups and the netns
//! template are recycled across launches, CLONE_INTO_CGROUP makes cgroup
//! placement atomic with `clone3` (with an `add_pid` fallback for nested
//! hosts), and the child writes its own id maps so no parent↔child pipe is
//! needed. argv/env are pre-built in the parent so the child never allocates.
//!
//! Only [`SandboxLauncher::launch`] requires elevated privileges
//! (CAP_SYS_ADMIN to create namespaces) and therefore is exercised in
//! integration tests rather than unit tests.
use super::{apparmor, caps, cgroup, clone3, drain, mounts, netns, seccomp, userns};
use crate::error::{SandboxError, SandboxResult};
use crate::spec::ChildSpec;
use parking_lot::Mutex;
use std::ffi::{CStr, CString};
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;
use tracing::instrument;

/// Upper bound on pooled cgroups. Beyond this, released cgroups are removed
/// from the filesystem instead of being recycled.
const MAX_POOLED_CGROUPS: usize = 32;

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

#[derive(Debug)]
pub struct SandboxLauncher {
    seccomp: seccomp::SeccompResolver,
    /// Reusable cgroups. Cgroup setup (mkdir + 3 limit writes + dirfd open)
    /// is ~12 syscalls per run; pooled cgroups pay only 3 limit `pwrite`s on
    /// reuse and the leak of `rockbox-<request_id>` dirs disappears.
    cgroups: Mutex<Vec<cgroup::Cgroup>>,
    cgroup_seq: AtomicU32,
    /// Recycled netns template, or `None` when we lack CAP_SYS_ADMIN (then
    /// every launch creates a fresh netns via CLONE_NEWNET as before).
    netns: Option<netns::NetnsTemplate>,
}

impl SandboxLauncher {
    pub fn new() -> SandboxResult<Self> {
        let netns = match netns::NetnsTemplate::init() {
            Ok(t) => Some(t),
            Err(e) => {
                tracing::warn!(?e, "netns_template_unavailable_using_clone_newnet");
                None
            }
        };
        Ok(Self {
            seccomp: seccomp::SeccompResolver::new(),
            cgroups: Mutex::new(Vec::new()),
            cgroup_seq: AtomicU32::new(0),
            netns,
        })
    }

    pub fn seccomp_resolver(&self) -> &seccomp::SeccompResolver {
        &self.seccomp
    }

    /// Return a cgroup to the pool for reuse by a later launch. Drains any
    /// stragglers (grandchildren) first so a recycled cgroup never carries
    /// live tasks or stale peak counters. A cgroup that had `cgroup.kill`
    /// written can never be reused: the kernel keeps CGRP_KILL set, and a
    /// child spawned into it via CLONE_INTO_CGROUP is SIGKILLed at birth.
    pub fn release_cgroup(&self, cg: cgroup::Cgroup) {
        if cg.was_killed() {
            let _ = cg.wait_empty();
            let _ = cg.remove();
            return;
        }
        if cg.is_populated().unwrap_or(false) {
            cg.kill_all().ok();
            if cg.wait_empty().is_err() {
                let _ = cg.remove();
                return;
            }
        }
        let _ = cg.reset_peaks();
        let mut pool = self.cgroups.lock();
        if pool.len() >= MAX_POOLED_CGROUPS {
            let _ = cg.remove();
        } else {
            pool.push(cg);
        }
    }

    fn take_cgroup(&self, spec: &ChildSpec) -> SandboxResult<cgroup::Cgroup> {
        let cfg = cgroup::CgroupConfig {
            limits: spec.limits,
        };
        let mut pool = self.cgroups.lock();
        if let Some(cg) = pool.pop() {
            match cg.configure(cfg) {
                Ok(()) => return Ok(cg),
                Err(e) => {
                    tracing::warn!(?e, "cgroup_reconfigure_failed");
                    let _ = cg.remove();
                }
            }
        }
        let name = format!(
            "rockbox-pool-{}",
            self.cgroup_seq.fetch_add(1, Ordering::Relaxed)
        );
        cgroup::Cgroup::create(&name, cfg)
    }

    /// Spawn a sandboxed child. Returns a [`ChildHandle`] for the parent
    /// to drain via [`drain::Drainer`].
    #[instrument(skip(self, spec), fields(req = %spec.request_id, lang = ?spec.language))]
    pub fn launch(&self, spec: &ChildSpec) -> SandboxResult<ChildHandle> {
        let cg = self.take_cgroup(spec)?;

        // Pre-create the pipes BEFORE clone (PERF — avoids post-fork allocation).
        let (stdout_r, stdout_w) = make_pipe()?;
        let (stderr_r, stderr_w) = make_pipe()?;
        // Sync pipe: the child blocks on its read end until the parent has
        // written the id maps into /proc/<child>/... (the child cannot write
        // its own maps inside its fresh PID namespace — EPERM).
        let (sync_r, sync_w) = make_pipe()?;

        let bpf = self.seccomp.compile(spec.seccomp_profile)?;

        // Pre-build argv/env in the parent. The child sees these via COW, so
        // building them here is free; building them in the child would fault
        // in live heap pages after the sync point.
        let argv = build_cstrings(spec.argv.iter());
        let env: Vec<CString> = spec
            .env
            .iter()
            .map(|(k, v)| CString::new(format!("{k}={v}")).expect("env must be UTF-8 sans NUL"))
            .collect();
        let argv_ptrs = ptr_array(&argv);
        let env_ptrs = ptr_array(&env);

        // Everything the child touches beyond raw syscalls is prebuilt here:
        // the mount plan (CStrings for every path/option), the AppArmor
        // changeprofile line, and the fexecve path. The child applies them
        // allocation-free; child-side `format!`/`PathBuf`/`CString::new`
        // would risk a glibc arena lock held by the main thread at clone3.
        let mount_plan = mounts::MountPlan::build(&spec.mounts)?;
        let apparmor_cmd = if apparmor::is_available() {
            Some(apparmor::change_command(&spec.apparmor_profile)?)
        } else {
            None
        };
        let fd_path = spec
            .binary_fd_path
            .as_ref()
            .map(|p| CString::new(p.as_bytes()).expect("fd path must be UTF-8 sans NUL"));

        let flags = clone3::CloneRequest::full_isolation();

        // CLONE_INTO_CGROUP places the child atomically; nested hosts that
        // refuse it fall back to a parent-side `add_pid`. clone3 is atomic,
        // so a failed CLONE_INTO_CGROUP attempt created no child — the retry
        // below is safe.
        //
        // The child branch runs INSIDE `clone_into_cgroup`'s netns-template
        // closure: if it returned normally, `with_template` would restore the
        // host netns — a `setns` that fails with EPERM in the child (fresh
        // user namespace) and would unwind the whole launch as an error.
        let (cloned, placed_via_clone) = self.clone_into_cgroup(&cg, flags, || {
            // CHILD. Fork of the calling thread (a tokio blocking-pool
            // worker); the netns template is inherited through exec. This
            // branch never returns.
            //
            // CRITICAL: the child is a fork of a multithreaded process. It
            // must not touch stdio, malloc arenas of other threads, or any
            // lock that another engine thread could hold at fork time — all
            // would deadlock forever. Diagnostics go straight to fd 2.
            child_diag("C0 fork-born");
            // SAFETY: the child's fd table is a private copy; closing the
            // pipe read-ends here does not affect the parent's copies.
            unsafe {
                libc::close(stdout_r.as_raw_fd());
                libc::close(stderr_r.as_raw_fd());
                libc::close(sync_w.as_raw_fd());
            }

            // Wait for the parent to write our id maps into /proc/<pid>/…
            // (see userns.rs — the child's own /proc/self writes EPERM inside
            // a new PID namespace). If the parent died first, the read
            // returns EOF and we bail out.
            if !userns::child_wait_for_maps(sync_r.as_raw_fd()) {
                child_diag("sync release failed");
                // SAFETY: _exit skips stdio flushing — the child must not
                // touch C stdio locks held by the engine's other threads.
                unsafe { libc::_exit(101) };
            }
            child_diag("C1 maps released");
            // SAFETY: setresuid/setresgid are standard syscalls; we have
            // CAP_SETUID in the new user namespace.
            unsafe {
                libc::setresgid(0, 0, 0);
                libc::setresuid(0, 0, 0);
            }
            child_diag("C2 creds done");

            let run = run_child(
                spec,
                &argv_ptrs,
                &env_ptrs,
                &mount_plan,
                apparmor_cmd.as_ref(),
                fd_path.as_ref(),
                stdout_w.as_raw_fd(),
                stderr_w.as_raw_fd(),
                &bpf,
            );
            if let Err(e) = run {
                // Raw errno print — no allocation in the child.
                let (errno, step) = match &e {
                    crate::error::SandboxError::Mount { step, source } => {
                        (source.raw_os_error().unwrap_or(-1), *step)
                    }
                    crate::error::SandboxError::RLimit { source, .. } => {
                        (source.raw_os_error().unwrap_or(-1), "")
                    }
                    _ => (-1, ""),
                };
                let mut buf = [0u8; 96];
                let prefix = b"child setup failed errno=";
                buf[..prefix.len()].copy_from_slice(prefix);
                let mut n = prefix.len();
                let mut v = errno.max(0) as u32;
                let mut digits = [0u8; 10];
                let mut d = 0;
                if v == 0 {
                    digits[d] = b'0';
                    d += 1;
                }
                while v > 0 {
                    digits[d] = b'0' + (v % 10) as u8;
                    v /= 10;
                    d += 1;
                }
                while d > 0 {
                    d -= 1;
                    buf[n] = digits[d];
                    n += 1;
                }
                for b in step.bytes() {
                    buf[n] = b;
                    n += 1;
                }
                buf[n] = b'\n';
                n += 1;
                // SAFETY: raw write to fd 2, statically-sized buffer.
                unsafe {
                    libc::write(2, buf.as_ptr() as *const libc::c_void, n);
                }
                // SAFETY: see above.
                unsafe { libc::_exit(102) };
            }
            unreachable!("execve should have replaced the process");
        })?;

        // PARENT.
        drop(stdout_w);
        drop(stderr_w);

        // The child is blocked on the sync pipe; write its id maps (the
        // child can't — EPERM inside its new PID ns) and release it.
        if let Err(e) = userns::parent_write_id_maps(cloned.child_pid) {
            tracing::warn!(?e, child = cloned.child_pid, "id_map_write_failed");
        }
        // SAFETY: write is stable; the sync pipe is open in this process.
        let _ = unsafe {
            libc::write(
                sync_w.as_raw_fd(),
                b"\x01".as_ptr() as *const libc::c_void,
                1,
            )
        };
        drop(sync_w);
        drop(sync_r);

        if !placed_via_clone {
            // Best-effort: nested-container hosts may not give us write access
            // to cgroup.procs, in which case the sandbox still runs (with
            // RLIMIT-only resource bounds) and we log.
            if let Err(e) = cg.add_pid(cloned.child_pid) {
                tracing::warn!(?e, child = cloned.child_pid, "cgroup_add_pid_failed");
            }
        }

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

    /// `clone3` with atomic cgroup placement where the host supports it.
    /// `child` runs in the fork child (inside the netns-template closure,
    /// where it must never return — the template's host-ns restore would
    /// fail in the child's new user namespace). Returns `(child, true)` when
    /// the child entered the cgroup via `CLONE_INTO_CGROUP`, else
    /// `(child, false)` — the caller must then `add_pid`.
    fn clone_into_cgroup<F>(
        &self,
        cg: &cgroup::Cgroup,
        flags: u64,
        child: F,
    ) -> SandboxResult<(clone3::Cloned, bool)>
    where
        F: FnOnce(),
    {
        let use_netns_template = self.netns.is_some();
        self.with_netns(|| {
            let req = clone3::CloneRequest {
                namespaces: flags_with_netns(flags, use_netns_template),
                cgroup_fd: Some(cg.fd()),
                exit_signal: libc::SIGCHLD as u64,
            };
            match clone3::clone3(req) {
                Ok(c) => {
                    if c.child_pid == 0 {
                        child();
                        unreachable!("child branch must not return");
                    }
                    Ok((c, true))
                }
                // Retry without CLONE_INTO_CGROUP — no child was created by
                // the failed attempt, so this is safe. clippy complains the
                // consume of req means rebuilding is fine; we rebuild anyway.
                Err(first_err) => {
                    tracing::info!(?first_err, "clone_into_cgroup_failed_fallback_add_pid");
                    let req = clone3::CloneRequest {
                        namespaces: flags_with_netns(flags, use_netns_template),
                        cgroup_fd: None,
                        exit_signal: libc::SIGCHLD as u64,
                    };
                    match clone3::clone3(req) {
                        Ok(c) => {
                            if c.child_pid == 0 {
                                child();
                                unreachable!("child branch must not return");
                            }
                            Ok((c, false))
                        }
                        Err(e) => Err(e),
                    }
                }
            }
        })
    }

    /// Run `f` inside the template netns if one exists, else in place with
    /// the full `CLONE_NEWNET` flag. The child branch of the wrapped `clone3`
    /// never returns here, so the host-ns restore is parent-only.
    fn with_netns<R>(&self, f: impl FnOnce() -> SandboxResult<R>) -> SandboxResult<R> {
        match &self.netns {
            Some(t) => t.with_template(f),
            None => f(),
        }
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

/// Strip `CLONE_NEWNET` when the template netns is in use (the child inherits
/// the template instead of asking the kernel to build a fresh namespace).
fn flags_with_netns(flags: u64, template: bool) -> u64 {
    if template {
        flags & !(libc::CLONE_NEWNET as u64)
    } else {
        flags
    }
}

fn build_cstrings<'a>(it: impl Iterator<Item = &'a String>) -> Vec<CString> {
    it.map(|s| CString::new(s.as_bytes()).expect("argv must be UTF-8 sans NUL"))
        .collect()
}

fn ptr_array(v: &[CString]) -> Vec<*const libc::c_char> {
    v.iter()
        .map(|c| c.as_ptr())
        .chain(std::iter::once(std::ptr::null()))
        .collect()
}

fn run_child(
    spec: &ChildSpec,
    argv_ptrs: &[*const libc::c_char],
    env_ptrs: &[*const libc::c_char],
    mount_plan: &mounts::MountPlan,
    apparmor_cmd: Option<&CString>,
    fd_path: Option<&CString>,
    stdout_w: RawFd,
    stderr_w: RawFd,
    bpf: &seccomp::BpfProfile,
) -> SandboxResult<()> {
    // 1. Redirect stdout/stderr to pipes. The write-ends are O_CLOEXEC, so
    // they vanish at exec; the child never returns to close them.
    redirect_to_pipe(stdout_w, libc::STDOUT_FILENO)?;
    redirect_to_pipe(stderr_w, libc::STDERR_FILENO)?;
    child_diag("C3 redirect done");

    // 2. Mount namespace setup (Layer 3) — prebuilt plan, raw syscalls.
    mount_plan.apply()?;
    child_diag("C4 mounts done");

    // 3. AppArmor (Layer 0/8) — staged for next exec.
    if let Some(cmd) = apparmor_cmd {
        apparmor::write_attr_exec(cmd)?;
    }
    child_diag("C5 apparmor done");

    // 4. NO_NEW_PRIVS (Layer 7) BEFORE seccomp (FIX SEC-14).
    caps::set_no_new_privs()?;
    if spec.layers.enforce_w_xor_x {
        let _ = caps::enable_mdwe();
    }
    child_diag("C6 nnp done");

    // 5. Seccomp (Layer 3 in §6). Stateless apply — the BPF blob was compiled
    // once at engine boot and is now just being reloaded into this task.
    seccomp::SeccompResolver::apply(bpf)?;
    child_diag("C7 seccomp done");

    // 6. Drop caps + rlimits (Layers 5 & 6).
    caps::drop_all_capabilities()?;
    child_diag("C8 caps done");
    caps::apply_rlimits(&spec.limits)?;
    child_diag("C9 rlimits done");

    // 7. execve via fexecve (verified fd) or pinned interpreter path.
    let exe = if let Some(p) = fd_path {
        p.as_c_str()
    } else if !argv_ptrs.is_empty() {
        // SAFETY: argv_ptrs[0] is a non-null CString from spec.argv.
        unsafe { CStr::from_ptr(argv_ptrs[0]) }
    } else {
        return Err(SandboxError::Internal("empty argv".into()));
    };
    // SAFETY: execve is a stable syscall; pointers above are NUL-terminated.
    unsafe {
        libc::execve(exe.as_ptr(), argv_ptrs.as_ptr(), env_ptrs.as_ptr());
    }
    // If we got here, execve failed.
    Err(SandboxError::Internal(format!(
        "execve: {}",
        std::io::Error::last_os_error()
    )))
}

/// Raw fd-2 write for the fork child. `eprintln!` is forbidden there: the
/// child inherits the engine's lock state, and stdio's global lock may be
/// held by a thread that doesn't exist in the child (permanent deadlock).
fn child_diag(msg: &str) {
    // SAFETY: write is stable; msg is a valid byte slice.
    unsafe {
        libc::write(2, msg.as_ptr() as *const libc::c_void, msg.len());
        libc::write(2, b"\n".as_ptr() as *const libc::c_void, 1);
    }
}

fn make_pipe() -> SandboxResult<(OwnedFd, OwnedFd)> {
    use nix::fcntl::OFlag;
    use nix::unistd::pipe2;
    let (r, w) = pipe2(OFlag::O_CLOEXEC | OFlag::O_NONBLOCK)
        .map_err(|e| SandboxError::Internal(format!("pipe2: {e}")))?;
    Ok((r, w))
}

fn redirect_to_pipe(pipe: RawFd, target_fd: libc::c_int) -> SandboxResult<()> {
    // SAFETY: dup2 is stable. The source fd stays open in the child until
    // exec (O_CLOEXEC) or _exit.
    let rc = unsafe { libc::dup2(pipe, target_fd) };
    if rc < 0 {
        return Err(SandboxError::Internal(format!(
            "dup2: {}",
            std::io::Error::last_os_error()
        )));
    }
    Ok(())
}
