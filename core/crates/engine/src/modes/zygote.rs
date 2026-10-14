//! Python exec zygote: one long-lived, already-booted Nix Python per engine
//! that never runs user code. Each request is a `fork()` of it, so the ~4–5 ms
//! interpreter boot that dominated `mode=exec` is paid once per engine, not
//! once per request.
//!
//! Isolation per request is kept, just applied by the fork instead of by
//! `execve`: the fork `unshare`s PID/mount/IPC/UTS namespaces, mounts fresh
//! tmpfs on `/sandbox` and `/tmp`, writes the request's files, sets
//! NO_NEW_PRIVS, installs the engine-compiled seccomp filter, drops every
//! capability, applies rlimits, optionally changes AppArmor profile, then
//! runs the program in-process (`runpy.run_path`, `__name__ == "__main__"`).
//! The zygote itself lives in the launcher's user/mount/net namespaces with
//! capabilities (it needs CAP_SYS_ADMIN for the per-fork namespaces) and only
//! ever executes the embedded shim. One engine serves one request at a time,
//! so consecutive forks never coexist; they share the zygote's (empty) netns
//! and user namespace exactly as consecutive per-request children shared the
//! engine's netns template before.
//!
//! Per request the engine hands the zygote a JSON frame plus three pipes
//! (`SCM_RIGHTS` over the fd-3 `SOCK_SEQPACKET` pair), places the fork into a
//! pooled cgroup by pid (found via `/proc/<zygote>/task/<zygote>/children`),
//! drains stdout/stderr, and receives the exit status as a final frame. Any
//! protocol failure kills the zygote and retries the request on the classic
//! `execve` path. `ROCKBOX_EXEC_ZYGOTE=0` disables the zygote entirely.

use crate::data_channel::{DataChannel, Stream};
use crate::modes::exec::ByteCap;
use crate::modes::work_root;
use crate::resolver::spec as resolver_spec;
use crate::state::EngineState;
use anyhow::{Context, Result, anyhow};
use base64::Engine as _;
use msgpack::FrameWriter;
use protocol::{FileEntry, Language, Mode, Response, ResultStatus, Settings};
use std::io::Write;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::time::{Duration, Instant};
use tokio::io::AsyncWrite;
use tracing::{debug, warn};

const SHIM_FILENAME: &str = ".rockbox_zygote.py";

pub struct Zygote {
    pid: i32,
    proto: OwnedFd,
    _pidfd: OwnedFd,
    cg: kernel::Cgroup,
    /// Runtime + memory shape this zygote was launched for.
    shape: String,
}

impl std::fmt::Debug for Zygote {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Zygote")
            .field("pid", &self.pid)
            .field("shape", &self.shape)
            .finish()
    }
}

impl Drop for Zygote {
    fn drop(&mut self) {
        let _ = self.cg.kill_all();
        let _ = self.cg.wait_empty();
        let _ = self.cg.remove();
        // SAFETY: zero-init siginfo_t; waitid on our own pidfd.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        unsafe {
            libc::waitid(
                libc::P_PIDFD,
                self._pidfd.as_raw_fd() as u32,
                &mut info,
                libc::WEXITED,
            );
        }
    }
}

fn enabled() -> bool {
    std::env::var("ROCKBOX_EXEC_ZYGOTE")
        .map(|v| v != "0")
        .unwrap_or(true)
}

/// Requests the zygote can serve: plain Python exec in the shared-kernel
/// sandbox with no network, no extra mounts and no capability that changes
/// the mount plan. Everything else keeps the `execve` path.
pub fn eligible(s: &Settings) -> bool {
    enabled()
        && s.mode == Mode::Exec
        && s.language == Language::Python
        && s.isolation == protocol::Isolation::Sandbox
        && s.network.tier == protocol::settings::NetworkTier::None
        && s.filesystem.mounts.is_empty()
        && !s.has_capability(protocol::Capability::Install)
        && !s.has_capability(protocol::Capability::Gpu)
}

fn shape_of(s: &Settings) -> String {
    format!(
        "{}:{}:{}",
        s.runtime.as_deref().unwrap_or("default"),
        s.limits.memory_mb,
        s.limits.cpu_cores
    )
}

pub async fn run<W: AsyncWrite + Unpin>(
    state: &EngineState,
    settings: Settings,
    writer: &FrameWriter<W>,
    data: Option<&DataChannel>,
) -> Result<()> {
    let launcher = match state.launcher.as_ref() {
        Some(l) => l.clone(),
        None => return crate::modes::exec::run(state, settings, writer, data).await,
    };
    let start = Instant::now();
    let request_id = settings.request_id.clone();
    let stream_enabled = settings.output.stream;
    let data_owned = data.cloned();
    let binary_cache = state.binary_cache.clone();

    let settings_for_block = settings.clone();
    let outcome = {
        let launcher = launcher.clone();
        let shape = shape_of(&settings);
        // The zygote slot is held for the whole request: one at a time.
        tokio::task::spawn_blocking(move || -> Result<Outcome> {
            let mut slot = state_zygote_lock();
            let zygote = ensure_zygote(
                &mut slot,
                &launcher,
                &binary_cache,
                &settings_for_block,
                &shape,
            )?;
            match serve(
                zygote,
                &launcher,
                &binary_cache,
                &settings_for_block,
                stream_enabled,
                data_owned.as_ref(),
            ) {
                Ok(o) => Ok(o),
                Err(e) => {
                    // A broken zygote is replaced on the next request.
                    warn!(error = %e, "zygote_request_failed_respawn");
                    *slot = None;
                    Err(e)
                }
            }
        })
        .await
        .context("zygote join")?
    };

    let outcome = match outcome {
        Ok(o) => o,
        Err(e) => {
            warn!(error = %e, "zygote_fallback_execve");
            return crate::modes::exec::run(state, settings, writer, data).await;
        }
    };
    let exec_ms = start.elapsed().as_millis() as u64;
    debug!(
        exec_ms,
        fork_ms = outcome.fork_ms,
        run_ms = outcome.run_ms,
        "zygote_exec_done"
    );
    writer
        .write(&Response::Result {
            request_id,
            status: outcome.status,
            exit_code: outcome.exit_code,
            exec_time_ms: exec_ms,
            memory_peak_mb: outcome.mem_peak_mb,
            cpu_time_ms: 0,
            output_bytes: outcome.output_total,
            output_truncated: matches!(outcome.status, ResultStatus::OutputExceeded),
            output: outcome.output.into_lossy_string(),
            errors: outcome.errors.into_lossy_string(),
        })
        .await?;
    Ok(())
}

/// One zygote per engine process; held for the duration of each request.
static ZYGOTE: parking_lot::Mutex<Option<Zygote>> = parking_lot::Mutex::new(None);

fn state_zygote_lock() -> parking_lot::MutexGuard<'static, Option<Zygote>> {
    ZYGOTE.lock()
}

struct Outcome {
    status: ResultStatus,
    exit_code: i32,
    output: ByteCap,
    errors: ByteCap,
    output_total: u64,
    mem_peak_mb: u64,
    fork_ms: u64,
    run_ms: u64,
}

fn ensure_zygote<'a>(
    slot: &'a mut Option<Zygote>,
    launcher: &kernel::SandboxLauncher,
    binary_cache: &cache::BinaryCache,
    settings: &Settings,
    shape: &str,
) -> Result<&'a mut Zygote> {
    if slot.as_ref().is_some_and(|z| z.shape != shape) {
        *slot = None;
    }
    if slot.is_none() {
        let t0 = Instant::now();
        let mut template = settings.clone();
        template.request_id = format!("zygote-{}", std::process::id());
        template.files = vec![FileEntry {
            path: SHIM_FILENAME.to_string(),
            content: python_shim().as_bytes().to_vec(),
            mode: 0o644,
        }];
        template.entrypoint = SHIM_FILENAME.to_string();
        template.stdin = None;
        template.env.insert("ROCKBOX_ZYGOTE".into(), "1".into());
        let mut resolved = resolver_spec::resolve(&template, work_root(), Some(binary_cache))
            .context("resolve zygote spec")?;
        resolved.spec.zygote = true;
        resolved.spec.protocol_fd = true;
        resolved.spec.limits.cpu_weight = crate::resolver::limits::CPU_WEIGHT_INTERACTIVE;
        // The zygote never exits by itself; the wall clock applies per fork.
        resolved.spec.wall_timeout = Duration::from_secs(365 * 24 * 3600);
        let handle = launcher.launch(&resolved.spec).context("launch zygote")?;
        let proto = handle
            .proto_fd
            .ok_or_else(|| anyhow!("zygote launched without protocol fd"))?;
        let z = Zygote {
            pid: handle.pid,
            proto,
            _pidfd: handle.pidfd,
            cg: handle.cgroup,
            shape: shape.to_string(),
        };
        // Hello frame = interpreter booted and shim listening.
        let hello = recv_frame(&z.proto, Duration::from_secs(20)).context("zygote hello")?;
        if hello != b"hello" {
            return Err(anyhow!(
                "bad zygote hello {:?}",
                String::from_utf8_lossy(&hello)
            ));
        }
        debug!(boot_ms = t0.elapsed().as_millis() as u64, "zygote_ready");
        *slot = Some(z);
    }
    Ok(slot.as_mut().expect("just set"))
}

/// One request through the zygote. Never leaves the fork running: on any
/// error the cgroup is killed before returning.
fn serve(
    z: &mut Zygote,
    launcher: &kernel::SandboxLauncher,
    binary_cache: &cache::BinaryCache,
    settings: &Settings,
    stream_enabled: bool,
    data: Option<&DataChannel>,
) -> Result<Outcome> {
    let t0 = Instant::now();
    // Resolve exactly as exec would, for the seccomp profile, limits, env and
    // AppArmor profile — the fork applies them itself.
    // Files travel in the frame; no work-dir writes for this request.
    let resolved = resolver_spec::resolve_opts(settings, work_root(), Some(binary_cache), false)
        .context("resolve spec")?;
    let spec = &resolved.spec;
    let bpf = launcher.seccomp_resolver().compile(spec.seccomp_profile)?;
    let cg = launcher.acquire_cgroup(spec)?;
    let b64 = base64::engine::general_purpose::STANDARD;
    let files: Vec<serde_json::Value> = settings
        .files
        .iter()
        .map(|f| serde_json::json!({"path": f.path, "content_b64": b64.encode(&f.content), "mode": f.mode}))
        .collect();
    let env: serde_json::Map<String, serde_json::Value> = spec
        .env
        .iter()
        .map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone())))
        .collect();
    let apparmor = if kernel::linux::apparmor::is_available() {
        Some(spec.apparmor_profile.clone())
    } else {
        None
    };
    let frame = serde_json::json!({
        "entry": settings.entrypoint,
        "files": files,
        "env": env,
        "seccomp_b64": b64.encode(bpf.raw_bytes()),
        "apparmor": apparmor,
        "rlimits": {
            "fsize": spec.limits.fsize_bytes,
            "nproc": spec.limits.pids_max,
            "nofile": spec.limits.nofile,
            "stack": spec.limits.stack_bytes,
        },
        "tmpfs_mb": settings.limits.tmpfs_mb,
        "sandbox_mb": 256,
    })
    .to_string();

    let (stdin_r, stdin_w) = pipe()?;
    let (stdout_r, stdout_w) = pipe()?;
    let (stderr_r, stderr_w) = pipe()?;
    send_frame_with_fds(
        &z.proto,
        frame.as_bytes(),
        &[
            stdin_r.as_raw_fd(),
            stdout_w.as_raw_fd(),
            stderr_w.as_raw_fd(),
        ],
    )?;
    drop(stdin_r);
    drop(stdout_w);
    drop(stderr_w);
    // stdin: hand the bytes over in the background so a large payload never
    // deadlocks against a child that writes first.
    if let Some(payload) = settings.stdin.as_ref() {
        let bytes = match payload {
            protocol::settings::StdinPayload::Text(t) => t.clone().into_bytes(),
            protocol::settings::StdinPayload::Bytes(b) => b.clone(),
        };
        std::thread::spawn(move || {
            let mut f = std::fs::File::from(stdin_w);
            let _ = f.write_all(&bytes);
        });
    } else {
        drop(stdin_w);
    }

    // The zygote forked; the middle process is its only live child. Place it
    // (and so the grandchild that runs user code) into the request cgroup,
    // then let it proceed.
    let forked = recv_frame(&z.proto, Duration::from_secs(5)).context("zygote forked frame")?;
    if forked != b"forked" {
        return Err(anyhow!(
            "unexpected zygote frame {:?}",
            String::from_utf8_lossy(&forked)
        ));
    }
    let middle = newest_child(z.pid).context("zygote child pid")?;
    cg.add_pid(middle).context("cgroup add fork")?;
    send_frame(&z.proto, b"go")?;
    let fork_ms = t0.elapsed().as_millis() as u64;

    let cap = settings.limits.output_bytes as usize;
    let mut output = ByteCap::new(cap);
    let mut errors = ByteCap::new(cap);
    let mut total: u64 = 0;
    let deadline = Instant::now() + spec.wall_timeout;
    let mut status_frame: Option<Vec<u8>> = None;
    let mut open = [true, true];
    let mut killed_for_timeout = false;
    let mut killed_for_output = false;
    let mut buf = vec![0u8; 64 * 1024];
    set_nonblocking(z.proto.as_raw_fd())?;
    while open[0] || open[1] || status_frame.is_none() {
        let remain = deadline.saturating_duration_since(Instant::now());
        if remain.is_zero() && !killed_for_timeout {
            killed_for_timeout = true;
            let _ = cg.kill_all();
        }
        let mut pfds = [
            libc::pollfd {
                fd: if open[0] { stdout_r.as_raw_fd() } else { -1 },
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: if open[1] { stderr_r.as_raw_fd() } else { -1 },
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: if status_frame.is_none() {
                    z.proto.as_raw_fd()
                } else {
                    -1
                },
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        let timeout_ms = if killed_for_timeout {
            2_000
        } else {
            remain.as_millis().min(60_000) as i32
        };
        // SAFETY: pollfd array is valid for the call.
        let rc = unsafe { libc::poll(pfds.as_mut_ptr(), 3, timeout_ms) };
        if rc < 0 {
            let e = std::io::Error::last_os_error();
            if e.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e.into());
        }
        if rc == 0 && killed_for_timeout {
            break; // grace period after kill expired; report as timeout
        }
        for i in 0..2 {
            if open[i] && pfds[i].revents != 0 {
                let fd = if i == 0 {
                    stdout_r.as_raw_fd()
                } else {
                    stderr_r.as_raw_fd()
                };
                // SAFETY: fd is a valid nonblocking pipe read end; buf is sized.
                let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
                if n > 0 {
                    let chunk = &buf[..n as usize];
                    total += n as u64;
                    if i == 0 {
                        output.extend(chunk);
                    } else {
                        errors.extend(chunk);
                    }
                    if stream_enabled {
                        if let Some(d) = data {
                            d.send(
                                if i == 0 {
                                    Stream::Stdout
                                } else {
                                    Stream::Stderr
                                },
                                chunk.to_vec(),
                            );
                        }
                    }
                    if total > settings.limits.output_bytes && !killed_for_output {
                        killed_for_output = true;
                        let _ = cg.kill_all();
                    }
                } else if n == 0
                    || (n < 0
                        && std::io::Error::last_os_error().kind() != std::io::ErrorKind::WouldBlock)
                {
                    open[i] = false;
                }
            }
        }
        if status_frame.is_none() && pfds[2].revents != 0 {
            match recv_frame_nonblocking(&z.proto) {
                Ok(Some(f)) => status_frame = Some(f),
                Ok(None) => {}
                Err(e) => return Err(e).context("zygote status"),
            }
        }
    }
    let run_ms = t0.elapsed().as_millis() as u64 - fork_ms;
    let status_json: serde_json::Value = status_frame
        .as_deref()
        .map(serde_json::from_slice)
        .transpose()
        .context("zygote status json")?
        .unwrap_or_default();
    if let Some(t) = status_json.get("timing") {
        debug!(timing = %t, "zygote_stage_ms");
    }
    let exit_code = status_json
        .get("exit_code")
        .and_then(|v| v.as_i64())
        .unwrap_or(-1) as i32;
    let signal = status_json
        .get("signal")
        .and_then(|v| v.as_i64())
        .unwrap_or(0) as i32;
    let oom = cg.oom_killed();
    let mem_peak_mb = cg.current_memory_peak().unwrap_or(0) / (1024 * 1024);
    let _ = cg.wait_empty();
    launcher.release_cgroup(cg);

    let (status, exit_code) = if killed_for_timeout {
        (ResultStatus::Timeout, -1)
    } else if killed_for_output {
        (ResultStatus::OutputExceeded, -1)
    } else if oom {
        (ResultStatus::OomKilled, -1)
    } else if signal != 0 {
        (ResultStatus::SeccompKill, -signal)
    } else if exit_code == 0 {
        (ResultStatus::Success, 0)
    } else {
        (ResultStatus::NonZeroExit, exit_code)
    };
    Ok(Outcome {
        status,
        exit_code,
        output,
        errors,
        output_total: total,
        mem_peak_mb,
        fork_ms,
        run_ms,
    })
}

/// Engine-namespace pid of the zygote's newest child (the per-request middle
/// process). The zygote serves one request at a time, so it is the only one.
fn newest_child(zygote_pid: i32) -> Result<i32> {
    let path = format!("/proc/{zygote_pid}/task/{zygote_pid}/children");
    for _ in 0..200 {
        let s = std::fs::read_to_string(&path)?;
        if let Some(pid) = s
            .split_whitespace()
            .last()
            .and_then(|p| p.parse::<i32>().ok())
        {
            return Ok(pid);
        }
        std::thread::sleep(Duration::from_micros(100));
    }
    Err(anyhow!("zygote reported a fork but /proc shows no child"))
}

fn pipe() -> Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0i32; 2];
    // SAFETY: pipe2 fills fds on success.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    // SAFETY: fresh fds owned here.
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

fn set_nonblocking(fd: i32) -> Result<()> {
    // SAFETY: fcntl on a valid fd.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

fn send_frame(sock: &OwnedFd, bytes: &[u8]) -> Result<()> {
    send_frame_with_fds(sock, bytes, &[])
}

/// One SEQPACKET message, optionally carrying fds via SCM_RIGHTS.
fn send_frame_with_fds(sock: &OwnedFd, bytes: &[u8], fds: &[i32]) -> Result<()> {
    let mut iov = libc::iovec {
        iov_base: bytes.as_ptr() as *mut libc::c_void,
        iov_len: bytes.len(),
    };
    let space = unsafe { libc::CMSG_SPACE((fds.len() * size_of::<i32>()) as u32) } as usize;
    let mut cbuf = vec![0u8; space];
    // SAFETY: msghdr/cmsghdr are filled per the sendmsg(2) contract.
    unsafe {
        let mut msg: libc::msghdr = std::mem::zeroed();
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        if !fds.is_empty() {
            msg.msg_control = cbuf.as_mut_ptr() as *mut libc::c_void;
            msg.msg_controllen = space as _;
            let cmsg = libc::CMSG_FIRSTHDR(&msg);
            (*cmsg).cmsg_level = libc::SOL_SOCKET;
            (*cmsg).cmsg_type = libc::SCM_RIGHTS;
            (*cmsg).cmsg_len = libc::CMSG_LEN((fds.len() * size_of::<i32>()) as u32) as _;
            std::ptr::copy_nonoverlapping(
                fds.as_ptr(),
                libc::CMSG_DATA(cmsg) as *mut i32,
                fds.len(),
            );
        }
        loop {
            let n = libc::sendmsg(sock.as_raw_fd(), &msg, 0);
            if n >= 0 {
                return Ok(());
            }
            let e = std::io::Error::last_os_error();
            if e.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            if e.kind() == std::io::ErrorKind::WouldBlock {
                std::thread::sleep(Duration::from_micros(200));
                continue;
            }
            return Err(e.into());
        }
    }
}

fn recv_frame(sock: &OwnedFd, timeout: Duration) -> Result<Vec<u8>> {
    let deadline = Instant::now() + timeout;
    loop {
        let mut pfd = libc::pollfd {
            fd: sock.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let remain = deadline.saturating_duration_since(Instant::now());
        if remain.is_zero() {
            return Err(anyhow!("zygote frame timeout"));
        }
        // SAFETY: single valid pollfd.
        let rc =
            unsafe { libc::poll(&mut pfd, 1, remain.as_millis().min(i32::MAX as u128) as i32) };
        if rc < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
            continue;
        }
        if rc <= 0 {
            return Err(anyhow!("zygote frame timeout"));
        }
        match recv_frame_nonblocking(sock)? {
            Some(f) => return Ok(f),
            None => continue,
        }
    }
}

/// `Ok(None)` when nothing is queued (EAGAIN); `Err` on a closed socket.
fn recv_frame_nonblocking(sock: &OwnedFd) -> Result<Option<Vec<u8>>> {
    let mut buf = vec![0u8; 64 * 1024];
    // SAFETY: recv into a sized buffer on a valid socket fd.
    let n = unsafe {
        libc::recv(
            sock.as_raw_fd(),
            buf.as_mut_ptr() as *mut libc::c_void,
            buf.len(),
            libc::MSG_DONTWAIT,
        )
    };
    if n < 0 {
        let e = std::io::Error::last_os_error();
        return if e.kind() == std::io::ErrorKind::WouldBlock {
            Ok(None)
        } else {
            Err(e.into())
        };
    }
    if n == 0 {
        return Err(anyhow!("zygote socket closed"));
    }
    buf.truncate(n as usize);
    Ok(Some(buf))
}

/// The zygote shim. Lives inside a Rust string: braces are doubled only where
/// the Rust side substitutes.
fn python_shim() -> &'static str {
    r#"import os, sys, json, base64, socket, array, ctypes, resource, runpy, signal, platform, traceback, time
PROTO = 3
sock = socket.socket(fileno=PROTO, family=socket.AF_UNIX, type=socket.SOCK_SEQPACKET)
libc = ctypes.CDLL(None, use_errno=True)
ARCH = platform.machine()
SYS_SECCOMP = {"aarch64": 277, "x86_64": 317}[ARCH]
SYS_CAPSET = {"aarch64": 91, "x86_64": 126}[ARCH]
CLONE_NEWNS, CLONE_NEWUTS, CLONE_NEWIPC, CLONE_NEWPID = 0x20000, 0x4000000, 0x8000000, 0x20000000
MS_REC, MS_PRIVATE, MS_NOSUID, MS_NODEV, MS_NOEXEC, MS_RDONLY = 0x4000, 0x40000, 2, 4, 8, 1
PR_SET_NO_NEW_PRIVS, PR_CAPBSET_DROP, PR_CAP_AMBIENT, PR_CAP_AMBIENT_CLEAR_ALL = 38, 24, 47, 4
SECCOMP_SET_MODE_FILTER, SECCOMP_FILTER_FLAG_TSYNC = 1, 1

def die(msg):
    os.write(2, (msg + "\n").encode())
    os._exit(126)

def check(rc, what):
    if rc != 0:
        e = ctypes.get_errno()
        die("zygote %s failed: %s" % (what, os.strerror(e)))

def unshare(flags):
    check(libc.unshare(flags), "unshare")

def mount(src, dst, fstype, flags, data):
    check(libc.mount(src.encode() if src else None, dst.encode(), fstype.encode() if fstype else None,
                     ctypes.c_ulong(flags), data.encode() if data else None), "mount " + dst)

def apply_seccomp(blob):
    class fprog(ctypes.Structure):
        _fields_ = [("len", ctypes.c_ushort), ("filter", ctypes.c_void_p)]
    buf = ctypes.create_string_buffer(blob, len(blob))
    prog = fprog(len(blob) // 8, ctypes.cast(buf, ctypes.c_void_p))
    check(libc.prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0), "no_new_privs")
    check(libc.syscall(SYS_SECCOMP, SECCOMP_SET_MODE_FILTER, SECCOMP_FILTER_FLAG_TSYNC, ctypes.byref(prog)), "seccomp")

def drop_caps():
    for cap in range(41):
        check(libc.prctl(PR_CAPBSET_DROP, cap, 0, 0, 0), "capbset_drop")
    check(libc.prctl(PR_CAP_AMBIENT, PR_CAP_AMBIENT_CLEAR_ALL, 0, 0, 0), "ambient_clear")
    hdr = (ctypes.c_uint32 * 2)(0x20080522, 0)
    data = (ctypes.c_uint32 * 6)()
    check(libc.syscall(SYS_CAPSET, ctypes.byref(hdr), ctypes.byref(data)), "capset")

def rlimits(r):
    resource.setrlimit(resource.RLIMIT_FSIZE, (r["fsize"], r["fsize"]))
    resource.setrlimit(resource.RLIMIT_NPROC, (r["nproc"], r["nproc"]))
    resource.setrlimit(resource.RLIMIT_NOFILE, (r["nofile"], r["nofile"]))
    resource.setrlimit(resource.RLIMIT_STACK, (r["stack"], r["stack"]))
    resource.setrlimit(resource.RLIMIT_CORE, (0, 0))

def child(req, fds, tfd, t_fork):
    # Grandchild: PID 1 of a fresh pid namespace, own mount/ipc/uts ns.
    stages = [("fork", t_fork), ("start", time.monotonic_ns())]
    os.dup2(fds[0], 0); os.dup2(fds[1], 1); os.dup2(fds[2], 2)
    for fd in fds: os.close(fd)
    sock.close()
    mount(None, "/", None, MS_REC | MS_PRIVATE, None)
    # Fresh /proc for the new pid namespace, else the program would see the
    # zygote's processes through the inherited mount.
    # Must be at least as restrictive as the launcher's locked /proc
    # (read-only, hidepid=2) or the kernel refuses it inside the user ns.
    mount("proc", "/proc", "proc", MS_NOSUID | MS_NODEV | MS_NOEXEC | MS_RDONLY, "hidepid=2")
    mount("tmpfs", "/sandbox", "tmpfs", MS_NOSUID | MS_NODEV, "size=%dm,mode=0755" % req.get("sandbox_mb", 256))
    mount("tmpfs", "/tmp", "tmpfs", MS_NOSUID | MS_NODEV, "size=%dm,mode=1777" % req.get("tmpfs_mb", 512))
    stages.append(("mounts", time.monotonic_ns()))
    for f in req["files"]:
        p = os.path.normpath(os.path.join("/sandbox", f["path"]))
        if not p.startswith("/sandbox/"):
            die("bad file path")
        os.makedirs(os.path.dirname(p), exist_ok=True)
        with open(p, "wb") as fh:
            fh.write(base64.b64decode(f["content_b64"]))
        os.chmod(p, int(f.get("mode", 0o644)) & 0o777)
    stages.append(("files", time.monotonic_ns()))
    # Same cwd as the execve path (`/`); the entry's directory is on sys.path.
    os.chdir("/")
    if req.get("apparmor"):
        try:
            with open("/proc/self/attr/current", "w") as fh:
                fh.write("changeprofile " + req["apparmor"] + "\n")
        except OSError as e:
            die("apparmor changeprofile failed: %s" % e)
    apply_seccomp(base64.b64decode(req["seccomp_b64"]))
    stages.append(("seccomp", time.monotonic_ns()))
    drop_caps()
    rlimits(req["rlimits"])
    stages.append(("caps", time.monotonic_ns()))
    # Stage report to the middle process before user code runs (the pipe is
    # closed afterwards so user code cannot reach it).
    os.write(tfd, json.dumps(stages).encode()); os.close(tfd)
    os.environ.clear()
    os.environ.update(req.get("env") or {})
    entry = os.path.join("/sandbox", req["entry"])
    sys.argv = [entry]
    sys.path[:] = ["/sandbox"] + [p for p in sys.path if p]
    sys.dont_write_bytecode = True
    code = 0
    try:
        runpy.run_path(entry, run_name="__main__")
    except SystemExit as e:
        code = e.code if isinstance(e.code, int) else (0 if e.code is None else 1)
        if not isinstance(e.code, (int, type(None))):
            sys.stderr.write(str(e.code) + "\n")
    except BaseException:
        traceback.print_exc()
        code = 1
    try:
        sys.stdout.flush(); sys.stderr.flush()
    except Exception:
        pass
    os._exit(code & 0xFF)

def middle(req, fds, go_r, t_fork, stat_w):
    # Unshare here so the grandchild is PID 1 of the new pid namespace, and
    # wait for the engine to place us in the request cgroup before forking.
    unshare(CLONE_NEWNS | CLONE_NEWIPC | CLONE_NEWUTS | CLONE_NEWPID)
    t_unshare = time.monotonic_ns()
    os.read(go_r, 2); os.close(go_r)
    t_go = time.monotonic_ns()
    tr, tw = os.pipe()
    pid = os.fork()
    if pid == 0:
        os.close(tr)
        child(req, fds, tw, t_fork)
    os.close(tw)
    for fd in fds: os.close(fd)
    report = os.read(tr, 4096); os.close(tr)
    _, status = os.waitpid(pid, 0)
    t_exit = time.monotonic_ns()
    try:
        stages = json.loads(report) if report else []
    except ValueError:
        stages = []
    stages = [("unshare", t_unshare), ("go", t_go)] + stages + [("exit", t_exit)]
    # Back to the zygote over a pipe: /tmp here is already the grandchild's
    # private tmpfs (same mount namespace), so a file would be invisible.
    os.write(stat_w, json.dumps(stages).encode()); os.close(stat_w)
    if os.WIFSIGNALED(status):
        signal.signal(os.WTERMSIG(status), signal.SIG_DFL)
        os.kill(os.getpid(), os.WTERMSIG(status))
    os._exit(os.WEXITSTATUS(status))

# Warm the interpreter once, in the zygote: runpy's lazy imports
# (importlib.util, pkgutil, io codecs, ...) cost ~3 ms on first use and
# every fork would otherwise pay them. Output goes to /dev/null.
_warm_fd = os.open(os.devnull, os.O_WRONLY)
_saved = os.dup(1); os.dup2(_warm_fd, 1)
try:
    _wp = "/tmp/.rockbox_warm.py"
    with open(_wp, "w") as _f:
        _f.write("import sys, os, json\nprint('warm')\n")
    runpy.run_path(_wp, run_name="__main__")
    os.unlink(_wp)
    try:
        raise ValueError("warm")
    except ValueError:
        traceback.format_exc()
finally:
    os.dup2(_saved, 1); os.close(_saved); os.close(_warm_fd)
# Everything allocated so far is permanent: freezing it keeps the collector
# from writing to inherited pages in every fork (copy-on-write churn).
import gc
gc.collect(); gc.freeze()

sock.send(b"hello")
while True:
    fds = array.array("i")
    msg, anc, flags, addr = sock.recvmsg(1 << 24, socket.CMSG_SPACE(3 * fds.itemsize))
    if not msg:
        break
    for level, typ, data in anc:
        if level == socket.SOL_SOCKET and typ == socket.SCM_RIGHTS:
            fds.frombytes(data[:len(data) - (len(data) % fds.itemsize)])
    req = json.loads(msg)
    go_r, go_w = os.pipe()
    stat_r, stat_w = os.pipe()
    t_fork = time.monotonic_ns()
    pid = os.fork()
    if pid == 0:
        os.close(go_w); os.close(stat_r)
        middle(req, list(fds), go_r, t_fork, stat_w)
    os.close(go_r); os.close(stat_w)
    for fd in fds: os.close(fd)
    sock.send(b"forked")
    go = sock.recv(16)
    os.write(go_w, b"go"); os.close(go_w)
    _, status = os.waitpid(pid, 0)
    exit_code = os.WEXITSTATUS(status) if os.WIFEXITED(status) else -1
    sig = os.WTERMSIG(status) if os.WIFSIGNALED(status) else 0
    timing = {}
    try:
        st = json.loads(os.read(stat_r, 4096) or b"[]")
        prev = st[0][1]
        for name, ts in st[1:]:
            timing[name] = round((ts - prev) / 1e6, 2); prev = ts
    except Exception:
        pass
    os.close(stat_r)
    sock.send(json.dumps({"exit_code": exit_code, "signal": sig, "timing": timing}).encode())
"#
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shim_applies_layers_in_launcher_order() {
        let s = python_shim();
        // Order of the *calls* inside the fork, not the helper definitions.
        let s = &s[s.find("def child(").unwrap()..];
        let order = [
            "mount(\"tmpfs\", \"/sandbox\"",
            "apparmor",
            "apply_seccomp(",
            "drop_caps()",
            "rlimits(req",
            "runpy.run_path",
        ];
        let mut last = 0;
        for needle in order {
            let i = s.find(needle).unwrap_or_else(|| panic!("missing {needle}"));
            assert!(i > last, "{needle} out of order");
            last = i;
        }
        assert!(s.contains("CLONE_NEWNS | CLONE_NEWIPC | CLONE_NEWUTS | CLONE_NEWPID"));
    }

    #[test]
    fn eligibility_is_python_exec_sandbox_only() {
        assert!(enabled() || std::env::var("ROCKBOX_EXEC_ZYGOTE").is_ok());
    }
}
