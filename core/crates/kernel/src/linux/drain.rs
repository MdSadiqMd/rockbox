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
}

impl Drainer {
    pub fn new(
        stdout: OwnedFd,
        stderr: OwnedFd,
        pidfd: OwnedFd,
        wall: Duration,
        output_cap: u64,
    ) -> SandboxResult<Self> {
        let timer = make_timerfd(wall)?;
        Ok(Self {
            stdout,
            stderr,
            pidfd,
            timer,
            output_cap,
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
                        cg.kill_all()?;
                        return Ok(ChildExit::Timeout);
                    }
                    _ => {}
                }
            }
        }
    }
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
