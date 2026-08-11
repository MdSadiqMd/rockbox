//! Mount-namespace setup. Executes in the **child** post-clone.
//!
//! Final filesystem inside the sandbox:
//!
//! /                  tmpfs   (new private root)
//! /nix/store         RO bind from host
//! /proc              proc, hidepid=2, no /proc/sys writes
//! /dev               /dev-min template (RO bind)  ← FIX SEC-10
//! /tmp               tmpfs (size from settings.limits.tmpfs_mb)
//! /sandbox           RO bind of user code work dir
//! /sandbox/main      RO bind of verified binary fd  (compiled only, FIX SEC-15)
//! /session           RW bind (mode=session)
//! /episode           RW bind (mode=rl_*)
//!
//! The plan is built in the **parent** ([`MountPlan::build`], allocation
//! allowed) so the fork child only runs raw syscalls. The child applies the
//! plan via [`MountPlan::apply`], which must stay allocation/lock-free — a
//! glibc arena lock held by another engine thread at `clone3` time stays
//! held in the child forever.
use crate::error::{SandboxError, SandboxResult};
use crate::spec::MountKind;
use std::ffi::{CStr, CString};

#[derive(Debug)]
pub enum MountStepKind {
    Tmpfs,
    Proc,
    Bind { ro: bool },
}

#[derive(Debug)]
pub struct MountStep {
    pub kind: MountStepKind,
    pub target: CString,
    pub source: CString,
    pub fstype: CString,
    pub options: CString,
}

#[derive(Debug)]
pub struct MountPlan {
    pub new_root: CString,
    pub steps: Vec<MountStep>,
}

impl MountPlan {
    /// Build the plan in the parent, where allocation is allowed. The child
    /// receives a fully pre-formatted, read-only view to `exec`.
    pub fn build(mounts: &[MountKind]) -> SandboxResult<Self> {
        let new_root = cstring("/tmp/rockbox-newroot", "new_root")?;
        let mut steps = Vec::with_capacity(mounts.len());
        for m in mounts {
            steps.push(build_step(&new_root, m)?);
        }
        Ok(Self { new_root, steps })
    }

    /// Apply the plan inside a freshly-cloned mount namespace. Raw syscalls
    /// only — must not allocate or touch any user-space lock.
    pub fn apply(&self) -> SandboxResult<()> {
        // 1. Make the new root a private tmpfs.
        mkdir_all(&self.new_root, "new_root mkdir")?;
        raw_mount(
            cstr(b"rockbox-root\0"),
            &self.new_root,
            cstr(b"tmpfs\0"),
            libc::MS_NOSUID,
            Some(cstr(b"mode=755,uid=0,gid=0\0")),
            "tmpfs root",
        )?;

        // 2. Apply each entry under the new root.
        for step in &self.steps {
            apply_step(step)?;
        }

        // 3. pivot_root.
        let mut put_old = [0u8; 4096];
        let root_len = self.new_root.to_bytes().len();
        if root_len + b"/.old_root".len() + 1 > put_old.len() {
            return Err(SandboxError::Mount {
                step: "put_old too long",
                source: std::io::Error::from_raw_os_error(libc::ENAMETOOLONG),
            });
        }
        put_old[..root_len].copy_from_slice(self.new_root.to_bytes());
        put_old[root_len..root_len + b"/.old_root".len()].copy_from_slice(b"/.old_root");
        let put_old = cstr_slice(&put_old[..=root_len + b"/.old_root".len()]);

        mkdir_all(put_old, "put_old mkdir")?;
        // SAFETY: pivot_root is a stable syscall; both paths are NUL-terminated.
        let rc = unsafe {
            libc::syscall(
                libc::SYS_pivot_root,
                self.new_root.as_ptr(),
                put_old.as_ptr(),
            )
        };
        if rc != 0 {
            return Err(SandboxError::Mount {
                step: "pivot_root",
                source: std::io::Error::last_os_error(),
            });
        }
        // SAFETY: chdir("/") is a stable syscall.
        if unsafe { libc::chdir(b"/\0".as_ptr() as *const libc::c_char) } != 0 {
            return Err(SandboxError::Mount {
                step: "chdir /",
                source: std::io::Error::last_os_error(),
            });
        }
        // SAFETY: umount2 is a stable syscall.
        if unsafe {
            libc::umount2(
                b"/.old_root\0".as_ptr() as *const libc::c_char,
                libc::MNT_DETACH,
            )
        } != 0
        {
            return Err(SandboxError::Mount {
                step: "umount old_root",
                source: std::io::Error::last_os_error(),
            });
        }
        // SAFETY: rmdir is a stable syscall.
        if unsafe { libc::rmdir(b"/.old_root\0".as_ptr() as *const libc::c_char) } != 0 {
            return Err(SandboxError::Mount {
                step: "rmdir old_root",
                source: std::io::Error::last_os_error(),
            });
        }

        // 4. Remount root RO (defence-in-depth).
        raw_mount(
            cstr(b"\0"),
            cstr(b"/\0"),
            cstr(b"\0"),
            libc::MS_REMOUNT | libc::MS_BIND | libc::MS_RDONLY,
            None,
            "remount root ro",
        )
    }
}

fn build_step(new_root: &CString, m: &MountKind) -> SandboxResult<MountStep> {
    match m {
        MountKind::Tmpfs {
            target,
            size_bytes,
            mode,
        } => Ok(MountStep {
            kind: MountStepKind::Tmpfs,
            target: join_root(new_root, target, "tmpfs target")?,
            source: cstring("rockbox-tmpfs", "tmpfs source")?,
            fstype: cstring("tmpfs", "tmpfs fstype")?,
            options: cstring(
                &format!("size={size_bytes},mode={mode:o},uid=0,gid=0"),
                "tmpfs opt",
            )?,
        }),
        MountKind::Proc { target } => Ok(MountStep {
            kind: MountStepKind::Proc,
            target: join_root(new_root, target, "proc target")?,
            source: cstring("proc", "proc source")?,
            fstype: cstring("proc", "proc fstype")?,
            options: cstring("hidepid=2", "proc opt")?,
        }),
        MountKind::BindRo { src, target } => bind_step(new_root, src, target, true),
        MountKind::BindRw { src, target } => bind_step(new_root, src, target, false),
        MountKind::DevMin { src, target } => bind_step(new_root, src, target, true),
    }
}

fn bind_step(
    new_root: &CString,
    src: &std::path::Path,
    target: &std::path::Path,
    ro: bool,
) -> SandboxResult<MountStep> {
    Ok(MountStep {
        kind: MountStepKind::Bind { ro },
        target: join_root(new_root, target, "bind target")?,
        source: cstring(&src.to_string_lossy(), "bind src")?,
        fstype: cstring("", "bind fstype")?,
        options: cstring("", "bind opt")?,
    })
}

fn apply_step(step: &MountStep) -> SandboxResult<()> {
    match step.kind {
        MountStepKind::Tmpfs => {
            mkdir_all(&step.target, "tmpfs mkdir")?;
            raw_mount(
                &step.source,
                &step.target,
                &step.fstype,
                libc::MS_NOSUID | libc::MS_NODEV,
                Some(&step.options),
                "tmpfs",
            )
        }
        MountStepKind::Proc => {
            mkdir_all(&step.target, "proc mkdir")?;
            raw_mount(
                &step.source,
                &step.target,
                &step.fstype,
                libc::MS_NOSUID | libc::MS_NODEV | libc::MS_RDONLY,
                Some(&step.options),
                "proc",
            )
        }
        MountStepKind::Bind { ro } => {
            let is_dir = is_dir(step.source.as_ptr());
            if is_dir {
                mkdir_all(&step.target, "bind mkdir")?;
            } else {
                // Create empty file as bind target. tmpfs in user-NS handles
                // mknod(S_IFREG) without surprises.
                mkdir_parent(&step.target)?;
                // SAFETY: mknod is a stable syscall.
                let rc = unsafe { libc::mknod(step.target.as_ptr(), libc::S_IFREG | 0o644, 0) };
                if rc != 0 {
                    let e = std::io::Error::last_os_error();
                    if e.raw_os_error() != Some(libc::EEXIST) {
                        return Err(SandboxError::Mount {
                            step: "bind touch",
                            source: e,
                        });
                    }
                }
            }
            raw_mount(
                &step.source,
                &step.target,
                &step.fstype,
                libc::MS_BIND | libc::MS_REC,
                None,
                "bind",
            )?;
            if ro {
                raw_mount(
                    cstr(b"\0"),
                    &step.target,
                    cstr(b"\0"),
                    libc::MS_REMOUNT | libc::MS_BIND | libc::MS_RDONLY | libc::MS_NOSUID,
                    None,
                    "bind remount-ro",
                )
            } else {
                Ok(())
            }
        }
    }
}

fn is_dir(path: *const libc::c_char) -> bool {
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: stat is a stable syscall; st is zeroed.
    let rc = unsafe { libc::stat(path, &mut st) };
    rc == 0 && (st.st_mode & libc::S_IFMT) == libc::S_IFDIR
}

/// `libc::mount` wrapper with C-string arguments, allocation-free.
#[allow(clippy::too_many_arguments)]
fn raw_mount(
    source: &CStr,
    target: &CStr,
    fstype: &CStr,
    flags: u64,
    options: Option<&CStr>,
    step: &'static str,
) -> SandboxResult<()> {
    let src = if source.to_bytes().is_empty() {
        std::ptr::null()
    } else {
        source.as_ptr()
    };
    let ty = if fstype.to_bytes().is_empty() {
        std::ptr::null()
    } else {
        fstype.as_ptr()
    };
    let data: *const libc::c_void = match options {
        Some(o) if !o.to_bytes().is_empty() => o.as_ptr() as *const libc::c_void,
        _ => std::ptr::null(),
    };
    // SAFETY: mount is a stable syscall; NULLs where no argument applies.
    if unsafe { libc::mount(src, target.as_ptr(), ty, flags, data) } != 0 {
        return Err(SandboxError::Mount {
            step,
            source: std::io::Error::last_os_error(),
        });
    }
    Ok(())
}

/// Iterative mkdir for possibly-multi-component paths on a stack buffer.
/// Same EOVERFLOW-avoidance reasoning as the old `std::fs::create_dir_all`
/// workaround: single `mkdir` syscalls, EEXIST tolerated.
fn mkdir_all(path: &CStr, step: &'static str) -> SandboxResult<()> {
    let bytes = path.to_bytes();
    let mut buf = [0u8; 4096];
    if bytes.len() >= buf.len() {
        return Err(SandboxError::Mount {
            step,
            source: std::io::Error::from_raw_os_error(libc::ENAMETOOLONG),
        });
    }
    buf[..bytes.len()].copy_from_slice(bytes);
    for i in 0..bytes.len() {
        if buf[i] == b'/' {
            if i == 0 {
                continue;
            }
            let saved = buf[i];
            buf[i] = 0;
            // SAFETY: buf is NUL-terminated at i.
            let rc = unsafe { libc::mkdir(buf.as_ptr() as *const libc::c_char, 0o755) };
            buf[i] = saved;
            if rc != 0 {
                let e = std::io::Error::last_os_error();
                if e.raw_os_error() != Some(libc::EEXIST) {
                    return Err(SandboxError::Mount { step, source: e });
                }
            }
        }
    }
    // SAFETY: final component, NUL-terminated at bytes.len().
    let rc = unsafe { libc::mkdir(buf.as_ptr() as *const libc::c_char, 0o755) };
    if rc != 0 {
        let e = std::io::Error::last_os_error();
        if e.raw_os_error() != Some(libc::EEXIST) {
            return Err(SandboxError::Mount { step, source: e });
        }
    }
    Ok(())
}

/// mkdir only the parent components of a path (for file bind targets).
fn mkdir_parent(path: &CStr) -> SandboxResult<()> {
    let bytes = path.to_bytes();
    match bytes.iter().rposition(|&b| b == b'/') {
        Some(i) if i > 0 => {
            let mut buf = [0u8; 4096];
            if i >= buf.len() {
                return Err(SandboxError::Mount {
                    step: "bind parent mkdir",
                    source: std::io::Error::from_raw_os_error(libc::ENAMETOOLONG),
                });
            }
            buf[..i].copy_from_slice(&bytes[..i]);
            buf[i] = 0;
            mkdir_all(cstr_slice(&buf[..=i]), "bind parent mkdir")
        }
        _ => Ok(()),
    }
}

fn join_root(
    new_root: &CString,
    target: &std::path::Path,
    what: &'static str,
) -> SandboxResult<CString> {
    use std::os::unix::ffi::OsStrExt;
    let stripped = target.strip_prefix("/").unwrap_or(target);
    let root_path = std::path::Path::new(std::ffi::OsStr::from_bytes(new_root.to_bytes()));
    let full = root_path.join(stripped);
    cstring(&full.to_string_lossy(), what)
}

fn cstring(s: &str, what: &'static str) -> SandboxResult<CString> {
    CString::new(s).map_err(|e| SandboxError::Mount {
        step: what,
        source: std::io::Error::other(e),
    })
}

fn cstr(bytes: &[u8]) -> &CStr {
    // SAFETY: static byte strings without interior NULs.
    unsafe { CStr::from_bytes_with_nul_unchecked(bytes) }
}

fn cstr_slice(bytes: &[u8]) -> &CStr {
    // SAFETY: caller guarantees a final NUL byte.
    unsafe { CStr::from_bytes_with_nul_unchecked(bytes) }
}
