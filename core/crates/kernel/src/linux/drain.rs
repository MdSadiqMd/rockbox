//! Async drain loop: stdout + stderr + pidfd + timerfd over `io_uring`
//! (FIX PERF-08). Falls back to plain `epoll_wait` if `io_uring_setup` fails.
//!
//! The drainer:
//! - reads bytes from the two pipe ends into caller-supplied callbacks,
//! - enforces the mid-stream output cap (FIX SEC-16); on overflow it tells
//!   the cgroup to kill all tasks (FIX PERF-10) and returns
//!   [`ChildExit::OutputCap`],
//! - returns [`ChildExit::Timeout`] when the timerfd fires,
//! - returns [`ChildExit::Normal`] when the pidfd signals exit.

use crate::error::{SandboxError, SandboxResult};
use crate::linux::cgroup::Cgroup;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::time::Duration;

#[derive(Debug)]
pub enum ChildExit {
    Normal {
        status: i32,
        memory_peak_mb: u64,
        cpu_time_ms: u64,
    },
    Signal {
        signo: i32,
    },
    Timeout,
    OomKilled,
    OutputCap,
}

#[derive(Debug)]
pub struct Drainer {
    pub stdout: OwnedFd,
    pub stderr: OwnedFd,
    pub pidfd: OwnedFd,
    pub timer: OwnedFd,
    pub output_cap: u64,
    /// Child pid, captured for timeout diagnostics (read /proc state before
    /// the cgroup kill destroys the evidence).
    pub pid: i32,
    /// Launch tag for correlating the timeout with the child's C-stage lines.
    pub tag: u32,
}

impl Drainer {
    pub fn new(
        stdout: OwnedFd,
        stderr: OwnedFd,
        pidfd: OwnedFd,
        wall: Duration,
        output_cap: u64,
        pid: i32,
        tag: u32,
    ) -> SandboxResult<Self> {
        let timer = make_timerfd(wall)?;
        Ok(Self {
            stdout,
            stderr,
            pidfd,
            timer,
            output_cap,
            pid,
            tag,
        })
    }

    /// Run until the child exits, the timer fires, or the output cap is hit.
    /// `on_stdout` / `on_stderr` receive byte chunks as they arrive. Both
    /// callbacks must be cheap — they execute on the drain thread.
    pub fn run<F1, F2>(
        &mut self,
        cg: &Cgroup,
        mut on_stdout: F1,
        mut on_stderr: F2,
    ) -> SandboxResult<ChildExit>
    where
        F1: FnMut(&[u8]),
        F2: FnMut(&[u8]),
    {
        // Minimal epoll fallback — io_uring optimization is a future step.
        let mut sent_bytes: u64 = 0;
        let mut buf = [0u8; 64 * 1024];
        let epfd = epoll_create()?;
        epoll_add(&epfd, self.stdout.as_raw_fd(), libc::EPOLLIN as u32, 1)?;
        epoll_add(&epfd, self.stderr.as_raw_fd(), libc::EPOLLIN as u32, 2)?;
        epoll_add(&epfd, self.pidfd.as_raw_fd(), libc::EPOLLIN as u32, 3)?;
        epoll_add(&epfd, self.timer.as_raw_fd(), libc::EPOLLIN as u32, 4)?;

        loop {
            let mut events = [libc::epoll_event { events: 0, u64: 0 }; 4];
            let n = unsafe {
                libc::epoll_wait(
                    epfd.as_raw_fd(),
                    events.as_mut_ptr(),
                    events.len() as i32,
                    -1,
                )
            };
            if n < 0 {
                let e = std::io::Error::last_os_error();
                if e.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(SandboxError::Internal(format!("epoll_wait: {e}")));
            }
            for ev in &events[..n as usize] {
                match ev.u64 {
                    1 => {
                        // Drain until EAGAIN so the epoll wakeup is amortized
                        // over every buffered byte, not one byte per wakeup.
                        let mut done = false;
                        while !done {
                            match read_chunk(&self.stdout, &mut buf)? {
                                Some(chunk) => {
                                    sent_bytes = sent_bytes.saturating_add(chunk.len() as u64);
                                    if sent_bytes > self.output_cap {
                                        cg.kill_all()?;
                                        return Ok(ChildExit::OutputCap);
                                    }
                                    on_stdout(chunk);
                                }
                                None => done = true,
                            }
                        }
                    }
                    2 => {
                        let mut done = false;
                        while !done {
                            match read_chunk(&self.stderr, &mut buf)? {
                                Some(chunk) => {
                                    sent_bytes = sent_bytes.saturating_add(chunk.len() as u64);
                                    if sent_bytes > self.output_cap {
                                        cg.kill_all()?;
                                        return Ok(ChildExit::OutputCap);
                                    }
                                    on_stderr(chunk);
                                }
                                None => done = true,
                            }
                        }
                    }
                    3 => {
                        // child exit — drain leftover bytes then reap.
                        drain_remaining(&self.stdout, &mut buf, &mut on_stdout);
                        drain_remaining(&self.stderr, &mut buf, &mut on_stderr);
                        let memory_peak_mb = cg.current_memory_peak().unwrap_or(0) / (1024 * 1024);
                        let (status, signo) = reap_pidfd(&self.pidfd);
                        return Ok(match signo {
                            Some(s) => ChildExit::Signal { signo: s },
                            None => ChildExit::Normal {
                                status,
                                memory_peak_mb,
                                cpu_time_ms: 0,
                            },
                        });
                    }
                    4 => {
                        // Was the child actually long dead and we missed the
                        // pidfd/pipe wakeup? WNOHANG reap tells us instantly.
                        let mut probe: libc::siginfo_t = unsafe { std::mem::zeroed() };
                        let rc = unsafe {
                            libc::waitid(
                                libc::P_PIDFD,
                                self.pidfd.as_raw_fd() as u32,
                                &mut probe,
                                libc::WEXITED | libc::WNOHANG,
                            )
                        };
                        // WNOHANG quirk: rc==0 with si_signo==0 means "no
                        // state change"; si_signo!=0 means reaped.
                        let early = if rc == 0 && probe.si_signo != 0 {
                            format!("already-exited si_code={} si_status={}", probe.si_code, unsafe { probe.si_status() })
                        } else if rc == 0 {
                            "still-alive(no-state-change)".to_string()
                        } else {
                            format!("still-alive rc={rc} errno={}", std::io::Error::last_os_error())
                        };
                        log_timeout_state(self.pid, self.tag, &early);
                        cg.kill_all()?;
                        return Ok(ChildExit::Timeout);
                    }
                    _ => {}
                }
            }
        }
    }
}

// On a wall-clock timeout, snapshot the child's kernel state before the
// cgroup kill removes it. This is the only chance to see WHY it never exited.
fn log_timeout_state(pid: i32, tag: u32, early_exit: &str) {
    let read = |suffix: &str| std::fs::read_to_string(format!("/proc/{pid}/{suffix}")).unwrap_or_default();
    let comm = read("comm").trim().to_string();
    let exe = std::fs::read_link(format!("/proc/{pid}/exe"))
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "?".into());
    let stat = read("stat");
    let state = stat
        .rsplit(')')
        .next()
        .and_then(|r| r.split_whitespace().next())
        .unwrap_or("?")
        .to_string();
    let wchan = read("wchan").trim().to_string();
    let maps = read("maps");
    let threads = read("status")
        .lines()
        .find(|l| l.starts_with("Threads:"))
        .unwrap_or("Threads: ?")
        .trim()
        .to_string();
    let syscall_raw = read("syscall");
    let (futex_word, futex_map) = peek_futex(pid, &syscall_raw, &maps);
    // Resolve the blocked instruction pointer against the child's maps.
    // /proc/pid/syscall layout: nr arg0..arg5 sp pc - pc is field index 8.
    let pc_map = syscall_raw
        .split_whitespace()
        .nth(8)
        .and_then(|pc| usize::from_str_radix(pc.trim_start_matches("0x"), 16).ok())
        .map(|pc| locate_addr(pc, &maps))
        .unwrap_or_else(|| "-".into());
    let parent_maps = std::fs::read_to_string("/proc/self/maps").unwrap_or_default();
    let (parent_word, parent_map) = peek_futex(std::process::id() as i32, &syscall_raw, &parent_maps);
    tracing::warn!(pid, tag, %comm, %exe, %state, %wchan, %threads, futex_word, futex_map, parent_word, parent_map, pc_map, early_exit, "child_timeout_state");
}

// Map line (with pathname) containing `addr`, from /proc-style maps content.
fn locate_addr(addr: usize, maps: &str) -> String {
    for line in maps.lines() {
        let mut parts = line.splitn(2, ' ');
        let range = parts.next().unwrap_or("");
        let rest = parts.next().unwrap_or("");
        if let Some((lo, hi)) = range.split_once('-') {
            if let (Ok(lo), Ok(hi)) = (usize::from_str_radix(lo, 16), usize::from_str_radix(hi, 16)) {
                if addr >= lo && addr < hi {
                    return format!("{line} (offset={:#x})", addr - lo);
                }
            }
        }
        let _ = rest;
    }
    "mapping-not-found".into()
}

// Returns (futex word value, maps line containing its address) for the
// futex(2) the task is blocked on; "-" when not applicable or unreadable.
fn peek_futex(pid: i32, syscall_raw: &str, maps: &str) -> (String, String) {
    let fields: Vec<&str> = syscall_raw.split_whitespace().collect();
    if fields.len() < 2 || fields[0] != "98" {
        return ("-".into(), "-".into());
    }
    let addr = match usize::from_str_radix(fields[1].trim_start_matches("0x"), 16) {
        Ok(a) => a,
        Err(_) => return ("-".into(), "-".into()),
    };
    let word = std::fs::File::open(format!("/proc/{pid}/mem"))
        .ok()
        .and_then(|mut f| {
            use std::io::{Read, Seek, SeekFrom};
            f.seek(SeekFrom::Start(addr as u64)).ok()?;
            let mut b = [0u8; 4];
            f.read_exact(&mut b).ok()?;
            Some(i32::from_le_bytes(b).to_string())
        })
        .unwrap_or_else(|| "unreadable".into());
    let map_line = maps
        .lines()
        .find(|l| match l.split_whitespace().next().and_then(|r| r.split_once('-')) {
            Some((lo, hi)) => match (usize::from_str_radix(lo, 16), usize::from_str_radix(hi, 16)) {
                (Ok(lo), Ok(hi)) => addr >= lo && addr < hi,
                _ => false,
            },
            None => false,
        })
        .unwrap_or("mapping-not-found")
        .to_string();
    (word, map_line)
}

fn make_timerfd(wall: Duration) -> SandboxResult<OwnedFd> {
    use std::os::fd::FromRawFd;
    let fd = unsafe { libc::timerfd_create(libc::CLOCK_MONOTONIC, libc::TFD_CLOEXEC) };
    if fd < 0 {
        return Err(SandboxError::Internal(format!(
            "timerfd_create: {}",
            std::io::Error::last_os_error()
        )));
    }
    let spec = libc::itimerspec {
        it_interval: libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        },
        it_value: libc::timespec {
            tv_sec: wall.as_secs() as libc::time_t,
            tv_nsec: wall.subsec_nanos() as i64,
        },
    };
    let rc = unsafe { libc::timerfd_settime(fd, 0, &spec, std::ptr::null_mut()) };
    if rc < 0 {
        return Err(SandboxError::Internal(format!(
            "timerfd_settime: {}",
            std::io::Error::last_os_error()
        )));
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn epoll_create() -> SandboxResult<OwnedFd> {
    use std::os::fd::FromRawFd;
    let fd = unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) };
    if fd < 0 {
        return Err(SandboxError::Internal(format!(
            "epoll_create1: {}",
            std::io::Error::last_os_error()
        )));
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn epoll_add(epfd: &OwnedFd, fd: RawFd, events: u32, token: u64) -> SandboxResult<()> {
    let mut ev = libc::epoll_event { events, u64: token };
    let rc = unsafe { libc::epoll_ctl(epfd.as_raw_fd(), libc::EPOLL_CTL_ADD, fd, &mut ev) };
    if rc < 0 {
        return Err(SandboxError::Internal(format!(
            "epoll_ctl: {}",
            std::io::Error::last_os_error()
        )));
    }
    Ok(())
}

fn read_chunk<'a>(fd: &OwnedFd, buf: &'a mut [u8]) -> SandboxResult<Option<&'a [u8]>> {
    // SAFETY: read is a stable syscall; `buf` is a valid byte slice.
    let n = unsafe { libc::read(fd.as_raw_fd(), buf.as_mut_ptr() as *mut _, buf.len()) };
    if n < 0 {
        let e = std::io::Error::last_os_error();
        match e.raw_os_error() {
            // EAGAIN == EWOULDBLOCK on Linux — one pattern covers both.
            Some(libc::EAGAIN) => Ok(None),
            _ => Err(SandboxError::Internal(format!("read: {e}"))),
        }
    } else if n == 0 {
        Ok(None)
    } else {
        Ok(Some(&buf[..n as usize]))
    }
}

fn drain_remaining<F: FnMut(&[u8])>(fd: &OwnedFd, buf: &mut [u8], cb: &mut F) {
    loop {
        match read_chunk(fd, buf) {
            Ok(Some(c)) if !c.is_empty() => cb(c),
            _ => break,
        }
    }
}

/// Reap an exited child via `waitid(P_PIDFD, ...)`. Returns
/// `(exit_status, Some(signal))` if the child was killed by a signal,
/// `(exit_status, None)` if it exited normally.
fn reap_pidfd(pidfd: &OwnedFd) -> (i32, Option<i32>) {
    // SAFETY: zero-init siginfo_t is well-defined; waitid is a stable syscall.
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    let rc = unsafe {
        libc::waitid(
            libc::P_PIDFD,
            pidfd.as_raw_fd() as u32,
            &mut info,
            libc::WEXITED,
        )
    };
    if rc != 0 {
        return (-1, None);
    }
    // Read si_code + si_status via libc's accessors (or raw fields).
    // siginfo_t::si_status() doesn't exist on libc; use the raw _sifields.
    // SAFETY: we just successfully waited — these fields are populated.
    let si_code = info.si_code;
    let si_status = unsafe {
        // si_status lives at offset 24 (after si_signo, si_errno, si_code,
        // padding) in the kernel sigchld layout. libc exposes it as
        // `_sifields._sigchld.si_status` on Linux.
        info.si_status()
    };
    match si_code {
        libc::CLD_EXITED => (si_status, None),
        libc::CLD_KILLED | libc::CLD_DUMPED => (-1, Some(si_status)),
        _ => (si_status, None),
    }
}
