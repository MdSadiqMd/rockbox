//! Mount-namespace setup. Executes in the **child** post-clone, after the
//! parent has signalled via the sync pipe.
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
use crate::error::{SandboxError, SandboxResult};
use crate::spec::MountKind;
use nix::mount::{MsFlags, mount, umount2};
use nix::unistd::{chdir, pivot_root};
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub struct MountPlan {
    pub new_root: PathBuf,
    pub entries: Vec<MountKind>,
}

impl MountPlan {
    pub fn new(new_root: impl Into<PathBuf>) -> Self {
        Self {
            new_root: new_root.into(),
            entries: Vec::new(),
        }
    }

    pub fn add(&mut self, m: MountKind) -> &mut Self {
        self.entries.push(m);
        self
    }

    /// Apply the plan inside a freshly-cloned mount namespace.
    pub fn apply(&self) -> SandboxResult<()> {
        let new_root = &self.new_root;

        // 1. Make the new root a private tmpfs.
        mkdir_lossy(new_root, "new_root mkdir")?;

        // `uid=0,gid=0` are mapped through the child's user namespace —
        // without them, tmpfs records the parent-NS root's id, which then
        // surfaces as `EOVERFLOW` from mkdir in this NS (kernel can't fit
        // the unmapped id into the inode owner field).
        mount::<_, _, _, str>(
            Some("rockbox-root"),
            new_root.as_path(),
            Some("tmpfs"),
            MsFlags::MS_NOSUID,
            Some("mode=755,uid=0,gid=0"),
        )
        .map_err(|e| io_mount("tmpfs root", e))?;

        // 2. Apply each entry inside the new root.
        for entry in &self.entries {
            apply_entry(new_root, entry)?;
        }

        // 3. pivot_root.
        let put_old = new_root.join(".old_root");
        mkdir_lossy(&put_old, "put_old mkdir")?;
        pivot_root(new_root.as_path(), put_old.as_path()).map_err(|e| io_mount("pivot_root", e))?;
        chdir("/").map_err(|e| io_mount("chdir /", e))?;
        umount2("/.old_root", nix::mount::MntFlags::MNT_DETACH)
            .map_err(|e| io_mount("umount old_root", e))?;
        std::fs::remove_dir("/.old_root").map_err(|e| SandboxError::Mount {
            step: "rmdir old_root",
            source: e,
        })?;

        // 4. Remount root RO (defence-in-depth).
        mount::<str, _, str, str>(
            None,
            "/",
            None,
            MsFlags::MS_REMOUNT | MsFlags::MS_BIND | MsFlags::MS_RDONLY,
            None,
        )
        .map_err(|e| io_mount("remount root ro", e))?;

        Ok(())
    }
}

fn apply_entry(new_root: &Path, entry: &MountKind) -> SandboxResult<()> {
    match entry {
        MountKind::Tmpfs {
            target,
            size_bytes,
            mode,
        } => {
            let tgt = new_root.join(strip_root(target));
            mkdir_lossy(&tgt, "tmpfs mkdir")?;
            let opt = format!("size={size_bytes},mode={mode:o},uid=0,gid=0");
            mount::<_, _, _, _>(
                Some("rockbox-tmpfs"),
                &tgt,
                Some("tmpfs"),
                MsFlags::MS_NOSUID | MsFlags::MS_NODEV,
                Some(opt.as_str()),
            )
            .map_err(|e| io_mount("tmpfs", e))?;
        }
        MountKind::BindRo { src, target } => {
            bind_one(new_root, src, target, true)?;
        }
        MountKind::BindRw { src, target } => {
            bind_one(new_root, src, target, false)?;
        }
        MountKind::Proc { target } => {
            let tgt = new_root.join(strip_root(target));
            mkdir_lossy(&tgt, "proc mkdir")?;
            mount::<_, _, _, _>(
                Some("proc"),
                &tgt,
                Some("proc"),
                MsFlags::MS_NOSUID | MsFlags::MS_NODEV | MsFlags::MS_RDONLY,
                Some("hidepid=2"),
            )
            .map_err(|e| io_mount("proc", e))?;
        }
        MountKind::DevMin { src, target } => {
            // Same as BindRo but explicit so reads of the audit log know
            // exactly which template was used.
            bind_one(new_root, src, target, true)?;
        }
    }
    Ok(())
}

fn bind_one(new_root: &Path, src: &Path, target: &Path, ro: bool) -> SandboxResult<()> {
    let tgt = new_root.join(strip_root(target));
    if src.is_dir() {
        mkdir_recursive(&tgt, "bind mkdir")?;
    } else {
        if let Some(p) = tgt.parent() {
            mkdir_recursive(p, "bind parent mkdir")?;
        }
        // Create empty file as bind target. `mknod(.., S_IFREG, 0)` is the
        // POSIX path; on tmpfs in user-NS it works without surprises.
        match nix::sys::stat::mknod(
            &tgt,
            nix::sys::stat::SFlag::S_IFREG,
            nix::sys::stat::Mode::from_bits_truncate(0o644),
            0,
        ) {
            Ok(()) => {}
            Err(nix::errno::Errno::EEXIST) => {}
            Err(e) => {
                return Err(SandboxError::Mount {
                    step: "bind touch",
                    source: std::io::Error::from_raw_os_error(e as i32),
                });
            }
        }
    }
    mount::<_, _, str, str>(
        Some(src),
        &tgt,
        None,
        MsFlags::MS_BIND | MsFlags::MS_REC,
        None,
    )
    .map_err(|e| io_mount("bind", e))?;
    if ro {
        mount::<str, _, str, str>(
            None,
            &tgt,
            None,
            MsFlags::MS_REMOUNT | MsFlags::MS_BIND | MsFlags::MS_RDONLY | MsFlags::MS_NOSUID,
            None,
        )
        .map_err(|e| io_mount("bind remount-ro", e))?;
    }
    Ok(())
}

fn strip_root(p: &Path) -> &Path {
    p.strip_prefix("/").unwrap_or(p)
}

fn io_mount(step: &'static str, e: nix::Error) -> SandboxError {
    SandboxError::Mount {
        step,
        source: std::io::Error::from_raw_os_error(e as i32),
    }
}

/// Single-component mkdir that swallows EEXIST. Avoids `std::fs::create_dir_all`
/// which internally `stat()`s and can hit EOVERFLOW under fresh user-NS + tmpfs
/// combos (the kernel reports the overflow_uid for unmapped ids and the stat
/// translation chokes on it).
fn mkdir_lossy(path: &Path, step: &'static str) -> SandboxResult<()> {
    match nix::unistd::mkdir(path, nix::sys::stat::Mode::from_bits_truncate(0o755)) {
        Ok(()) => Ok(()),
        Err(nix::errno::Errno::EEXIST) => Ok(()),
        Err(e) => Err(SandboxError::Mount {
            step,
            source: std::io::Error::from_raw_os_error(e as i32),
        }),
    }
}

/// Iterative mkdir for paths that may have multiple missing components.
/// Same EOVERFLOW-avoidance reasoning as [`mkdir_lossy`].
fn mkdir_recursive(path: &Path, step: &'static str) -> SandboxResult<()> {
    let mut acc = std::path::PathBuf::new();
    for comp in path.components() {
        acc.push(comp);
        if acc.as_os_str().is_empty() || acc == std::path::PathBuf::from("/") {
            continue;
        }
        mkdir_lossy(&acc, step)?;
    }
    Ok(())
}
