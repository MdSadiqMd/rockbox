//! `isolation = microvm`: one Firecracker microVM per exec request.
//!
//! The VM-grade tier for hostile multi-tenant traffic. Each request boots
//! (or restores) a guest with its own kernel, hands the program to a tiny
//! vsock agent (`microvm/guest-agent.py`), and tears the VM down. Assets
//! (`firecracker`, `vmlinux`, `rootfs.ext4`) come from
//! `ROCKBOX_MICROVM_DIR` (default `/var/lib/sandbox/microvm`,
//! `microvm/setup-assets.sh`); the golden rootfs is attached read-only and
//! the guest init mounts tmpfs on `/sandbox` and `/tmp`, so no per-VM disk
//! copy exists. `/dev/kvm` is required — the engine reports
//! `microvm_unavailable` otherwise instead of silently falling back to the
//! shared-kernel sandbox (a caller asked for VM isolation).
//!
//! Python only for now: the guest image carries Ubuntu's python3, not the
//! nix runtime catalog. Firecracker's API is spoken over its unix socket
//! with a hand-rolled HTTP/1.1 client (std only — no new dependency for
//! five fixed requests).
//!
//! Per-request VMs are *restored* from a golden snapshot (booted guest with
//! the agent idle) rather than cold-booted: the guest kernel alone needs
//! ~150 ms on real KVM and ~1.7 s under nested virtualization, while a
//! restore is tens of ms. The snapshot bakes in the vsock UDS path, so every
//! firecracker process runs in its own mount namespace with its work dir
//! bind-mounted over [`FIXED_DIR`]; the host side talks to the real file in
//! the work dir. The golden VM itself runs *without* the bind so the path
//! Firecracker records in the snapshot is the fixed one (it stores the
//! resolved host path, not the configured string). Restores share the golden memory file copy-on-write, so
//! requests never see each other's state. `ROCKBOX_MICROVM_SNAPSHOT=0`
//! forces cold boots (A/B).

use crate::data_channel::DataChannel;
use crate::state::EngineState;
use anyhow::{Context, Result, anyhow};
use base64::Engine as _;
use msgpack::FrameWriter;
use protocol::{Language, Response, ResultStatus, Settings};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use tokio::io::AsyncWrite;
use tracing::debug;

const AGENT_PORT: u32 = 5000;
// No serial console: a verbose console cost +1.5 s of boot under test and
// nothing reads it in production.
const BOOT_ARGS: &str =
    "reboot=k panic=1 pci=off root=/dev/vda ro init=/sbin/rockbox-init quiet 8250.nr_uarts=0";
/// Path the vsock UDS has inside every firecracker mount namespace.
const FIXED_DIR: &str = "/run/rockbox-microvm";

fn snapshot_enabled() -> bool {
    std::env::var("ROCKBOX_MICROVM_SNAPSHOT")
        .map(|v| v != "0")
        .unwrap_or(true)
}

/// Golden snapshots are shared by every engine process on the host: one per
/// (vcpus, mem) shape under `work_root()/microvm-golden/<shape>/`, built by
/// whichever engine gets the directory lock first and published atomically.
/// Without sharing, a burst of N concurrent microvm requests made N engines
/// each boot + snapshot their own guest (3 s and a full memory file apiece).
fn golden_for(vcpus: u32, mem: u64) -> Result<Option<(PathBuf, PathBuf)>> {
    if !snapshot_enabled() {
        return Ok(None);
    }
    let dir = super::work_root()
        .join("microvm-golden")
        .join(format!("{vcpus}c-{mem}m"));
    let snap = dir.join("snapshot.bin");
    let mem_file = dir.join("memory.bin");
    if snap.exists() && mem_file.exists() {
        return Ok(Some((snap, mem_file)));
    }
    std::fs::create_dir_all(&dir)?;
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(dir.join(".lock"))?;
    // SAFETY: flock on a valid fd; blocks until the builder (if any) finishes.
    if unsafe { libc::flock(std::os::fd::AsRawFd::as_raw_fd(&lock), libc::LOCK_EX) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    if snap.exists() && mem_file.exists() {
        return Ok(Some((snap, mem_file)));
    }
    let t0 = Instant::now();
    let build = dir.join(format!("build-{}", std::process::id()));
    let mut vm = Vm::spawn(&assets_dir(), &build, false)?;
    vm.configure_and_boot(vcpus, mem)?;
    vm.agent(
        &json_req("ready", serde_json::json!({})),
        Duration::from_secs(60),
    )?;
    vm.api_patch("/vm", &serde_json::json!({"state": "Paused"}))?;
    let tmp_snap = build.join("snapshot.bin");
    let tmp_mem = build.join("memory.bin");
    vm.api_put(
        "/snapshot/create",
        &serde_json::json!({"snapshot_type": "Full", "snapshot_path": tmp_snap, "mem_file_path": tmp_mem}),
    )?;
    // Publish before the VM (and its work dir) is torn down.
    std::fs::rename(&tmp_mem, &mem_file)?;
    std::fs::rename(&tmp_snap, &snap)?;
    drop(vm);
    debug!(
        golden_ms = t0.elapsed().as_millis() as u64,
        vcpus, mem, "microvm_golden_ready"
    );
    Ok(Some((snap, mem_file)))
}

fn shape(settings: &Settings) -> (u32, u64) {
    (
        (settings.limits.cpu_cores.ceil() as u32).clamp(1, 8),
        settings.limits.memory_mb.clamp(128, 32_768),
    )
}

pub fn assets_dir() -> PathBuf {
    std::env::var_os("ROCKBOX_MICROVM_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/var/lib/sandbox/microvm"))
}

/// Why the tier cannot run here, or `None` when it can.
pub fn unavailable_reason() -> Option<String> {
    let dir = assets_dir();
    if !Path::new("/dev/kvm").exists() {
        return Some("/dev/kvm not present".into());
    }
    for f in ["firecracker", "vmlinux", "rootfs.ext4"] {
        if !dir.join(f).exists() {
            return Some(format!(
                "missing {} (run microvm/setup-assets.sh)",
                dir.join(f).display()
            ));
        }
    }
    None
}

pub async fn run<W: AsyncWrite + Unpin>(
    _state: &EngineState,
    settings: Settings,
    writer: &FrameWriter<W>,
    _data: Option<&DataChannel>,
) -> Result<()> {
    let request_id = settings.request_id.clone();
    let start = Instant::now();
    let fail = |status: ResultStatus, msg: String, start: Instant| Response::Result {
        request_id: request_id.clone(),
        status,
        exit_code: -1,
        exec_time_ms: start.elapsed().as_millis() as u64,
        memory_peak_mb: 0,
        cpu_time_ms: 0,
        output_bytes: 0,
        output_truncated: false,
        output: String::new(),
        errors: msg,
    };
    if let Some(reason) = unavailable_reason() {
        writer
            .write(&fail(
                ResultStatus::EngineError,
                format!("microvm_unavailable: {reason}"),
                start,
            ))
            .await?;
        return Ok(());
    }
    if settings.language != Language::Python {
        writer
            .write(&fail(
                ResultStatus::EngineError,
                "microvm_unsupported_language: the microVM guest image carries python only".into(),
                start,
            ))
            .await?;
        return Ok(());
    }

    let workdir = super::work_root().join(format!("microvm-{}", settings.request_id));
    let result = tokio::task::spawn_blocking(move || run_blocking(&settings, &workdir))
        .await
        .context("microvm join")?;
    let resp = match result {
        Ok(r) => Response::Result {
            request_id,
            status: r.status,
            exit_code: r.exit_code,
            exec_time_ms: start.elapsed().as_millis() as u64,
            memory_peak_mb: r.rss_mb,
            cpu_time_ms: 0,
            output_bytes: (r.output.len() + r.errors.len()) as u64,
            output_truncated: r.truncated,
            output: r.output,
            errors: r.errors,
        },
        Err(e) => fail(
            ResultStatus::EngineError,
            format!("microvm_failed: {e:#}"),
            start,
        ),
    };
    writer.write(&resp).await?;
    Ok(())
}

struct Outcome {
    status: ResultStatus,
    exit_code: i32,
    output: String,
    errors: String,
    truncated: bool,
    rss_mb: u64,
}

fn run_blocking(settings: &Settings, workdir: &Path) -> Result<Outcome> {
    std::fs::create_dir_all(workdir)?;
    let (vcpus, mem) = shape(settings);
    let golden = golden_for(vcpus, mem).unwrap_or_else(|e| {
        tracing::warn!(error = %e, "microvm_golden_failed_cold_boot");
        None
    });
    let mut vm = Vm::spawn(&assets_dir(), workdir, true)?;
    let t_boot = Instant::now();
    let restored = match &golden {
        Some((snap, mem_file)) => {
            vm.api_put(
                "/snapshot/load",
                &serde_json::json!({"snapshot_path": snap, "mem_backend": {"backend_path": mem_file, "backend_type": "File"}, "resume_vm": true}),
            )?;
            true
        }
        None => {
            vm.configure_and_boot(vcpus, mem)?;
            false
        }
    };
    let wall = Duration::from_millis(settings.limits.wall_ms.max(1_000));
    vm.agent(
        &json_req("ready", serde_json::json!({})),
        Duration::from_secs(20),
    )?;
    let boot_ms = t_boot.elapsed().as_millis() as u64;

    let b64 = base64::engine::general_purpose::STANDARD;
    let files: Vec<serde_json::Value> = settings
        .files
        .iter()
        .map(|f| {
            serde_json::json!({"path": f.path, "content_b64": b64.encode(&f.content), "mode": f.mode})
        })
        .collect();
    vm.agent(
        &json_req(
            "write_files",
            serde_json::json!({"root": "/sandbox", "files": files}),
        ),
        Duration::from_secs(20),
    )?;
    let env: serde_json::Map<String, serde_json::Value> = settings
        .env
        .iter()
        .map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone())))
        .collect();
    let t_exec = Instant::now();
    let run = vm.agent(
        &json_req(
            "run",
            serde_json::json!({
                "argv": ["/usr/bin/python3", format!("/sandbox/{}", settings.entrypoint)],
                "cwd": "/sandbox",
                "env": env,
                "stdin_b64": settings.stdin.as_ref().map(|s| match s {
                    protocol::settings::StdinPayload::Text(t) => b64.encode(t.as_bytes()),
                    protocol::settings::StdinPayload::Bytes(b) => b64.encode(b),
                }),
                "timeout_ms": settings.limits.wall_ms,
                "output_bytes": settings.limits.output_bytes,
            }),
        ),
        wall + Duration::from_secs(5),
    )?;
    let exec_ms = t_exec.elapsed().as_millis() as u64;
    let rss_mb = vm.rss_kib() / 1024;
    debug!(boot_ms, exec_ms, rss_mb, restored, "microvm_done");

    let get_b64 = |k: &str| {
        run.get(k)
            .and_then(|v| v.as_str())
            .and_then(|s| b64.decode(s).ok())
            .map(|b| String::from_utf8_lossy(&b).into_owned())
            .unwrap_or_default()
    };
    let exit_code = run.get("exit_code").and_then(|v| v.as_i64()).unwrap_or(-1) as i32;
    let timed_out = run
        .get("timed_out")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let truncated = run
        .get("truncated")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let status = if timed_out {
        ResultStatus::Timeout
    } else if truncated {
        ResultStatus::OutputExceeded
    } else if exit_code == 0 {
        ResultStatus::Success
    } else {
        ResultStatus::NonZeroExit
    };
    Ok(Outcome {
        status,
        exit_code,
        output: get_b64("stdout_b64"),
        errors: get_b64("stderr_b64"),
        truncated,
        rss_mb,
    })
}

fn json_req(method: &str, params: serde_json::Value) -> String {
    serde_json::json!({"id": 1, "method": method, "params": params}).to_string()
}

struct Vm {
    child: Child,
    api: PathBuf,
    vsock: PathBuf,
    assets: PathBuf,
    workdir: PathBuf,
}

impl Vm {
    /// `private_vsock_dir`: run in a mount namespace with `workdir` bound over
    /// [`FIXED_DIR`] (per-request VMs). The golden VM passes `false` so the
    /// socket really lives at the fixed path and the snapshot records it.
    fn spawn(assets: &Path, workdir: &Path, private_vsock_dir: bool) -> Result<Self> {
        std::fs::create_dir_all(workdir)?;
        std::fs::create_dir_all(FIXED_DIR)?;
        let api = workdir.join("fc.sock");
        let vsock = if private_vsock_dir {
            workdir.join("vsock.sock")
        } else {
            let p = Path::new(FIXED_DIR).join("vsock.sock");
            let _ = std::fs::remove_file(&p);
            p
        };
        let bind_src = std::ffi::CString::new(workdir.as_os_str().as_encoded_bytes())?;
        let bind_dst = std::ffi::CString::new(FIXED_DIR)?;
        let mut cmd = Command::new(assets.join("firecracker"));
        cmd.arg("--api-sock")
            .arg(&api)
            .arg("--level")
            .arg("Error")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        // SAFETY: only raw syscalls between fork and exec (no allocation).
        if private_vsock_dir {
            unsafe {
                use std::os::unix::process::CommandExt;
                cmd.pre_exec(move || {
                    if libc::unshare(libc::CLONE_NEWNS) != 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    let root = c"/";
                    if libc::mount(
                        std::ptr::null(),
                        root.as_ptr(),
                        std::ptr::null(),
                        libc::MS_REC | libc::MS_PRIVATE,
                        std::ptr::null(),
                    ) != 0
                    {
                        return Err(std::io::Error::last_os_error());
                    }
                    if libc::mount(
                        bind_src.as_ptr(),
                        bind_dst.as_ptr(),
                        std::ptr::null(),
                        libc::MS_BIND,
                        std::ptr::null(),
                    ) != 0
                    {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
        }
        let child = cmd.spawn().context("spawn firecracker")?;
        let deadline = Instant::now() + Duration::from_secs(5);
        while !api.exists() {
            if Instant::now() > deadline {
                return Err(anyhow!("firecracker API socket never appeared"));
            }
            std::thread::sleep(Duration::from_micros(200));
        }
        Ok(Self {
            child,
            api,
            vsock,
            assets: assets.to_path_buf(),
            workdir: workdir.to_path_buf(),
        })
    }

    fn configure_and_boot(&mut self, vcpus: u32, mem: u64) -> Result<()> {
        self.api_put(
            "/machine-config",
            &serde_json::json!({"vcpu_count": vcpus, "mem_size_mib": mem, "smt": false}),
        )?;
        self.api_put(
            "/boot-source",
            &serde_json::json!({"kernel_image_path": self.assets.join("vmlinux"), "boot_args": BOOT_ARGS}),
        )?;
        self.api_put(
            "/drives/rootfs",
            &serde_json::json!({"drive_id": "rootfs", "path_on_host": self.assets.join("rootfs.ext4"), "is_root_device": true, "is_read_only": true}),
        )?;
        self.api_put(
            "/vsock",
            &serde_json::json!({"guest_cid": 3, "uds_path": self.vsock}),
        )?;
        self.api_put(
            "/actions",
            &serde_json::json!({"action_type": "InstanceStart"}),
        )?;
        Ok(())
    }

    /// Minimal HTTP/1.1 over the API unix socket; `Connection: close` so the
    /// response is simply "read to EOF".
    fn api_put(&self, path: &str, body: &serde_json::Value) -> Result<()> {
        self.api_call("PUT", path, body)
    }

    fn api_patch(&self, path: &str, body: &serde_json::Value) -> Result<()> {
        self.api_call("PATCH", path, body)
    }

    fn api_call(&self, method: &str, path: &str, body: &serde_json::Value) -> Result<()> {
        let body = body.to_string();
        let mut s = UnixStream::connect(&self.api).context("connect firecracker api")?;
        s.set_read_timeout(Some(Duration::from_secs(60)))?;
        write!(
            s,
            "{method} {path} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        )?;
        // Firecracker keeps the connection alive regardless of
        // `Connection: close`, so read the status line + headers and exactly
        // `Content-Length` body bytes instead of waiting for EOF.
        let mut r = BufReader::new(s);
        let mut status_line = String::new();
        r.read_line(&mut status_line)
            .context("firecracker status line")?;
        let status: u16 = status_line
            .split_whitespace()
            .nth(1)
            .and_then(|c| c.parse().ok())
            .unwrap_or(0);
        let mut content_length = 0usize;
        loop {
            let mut line = String::new();
            if r.read_line(&mut line)? == 0 || line == "\r\n" || line == "\n" {
                break;
            }
            if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                content_length = v.trim().parse().unwrap_or(0);
            }
        }
        let mut resp = vec![0u8; content_length];
        if content_length > 0 {
            r.read_exact(&mut resp)?;
        }
        if !(200..300).contains(&status) {
            return Err(anyhow!(
                "firecracker {path}: {status} {}",
                String::from_utf8_lossy(&resp)
            ));
        }
        Ok(())
    }

    /// One newline-JSON request to the guest agent over Firecracker's vsock
    /// UDS (`CONNECT <port>` handshake). Retries the connect until the guest
    /// is listening or `timeout` passes — this is how boot-to-ready is
    /// awaited.
    fn agent(&self, request: &str, timeout: Duration) -> Result<serde_json::Value> {
        let deadline = Instant::now() + timeout;
        let mut stream = loop {
            match UnixStream::connect(&self.vsock) {
                Ok(mut s) => {
                    s.set_read_timeout(Some(Duration::from_millis(200)))?;
                    if s.write_all(format!("CONNECT {AGENT_PORT}\n").as_bytes())
                        .is_ok()
                    {
                        let mut ack = String::new();
                        let mut r = BufReader::new(s.try_clone()?);
                        if r.read_line(&mut ack).is_ok() && ack.starts_with("OK") {
                            break s;
                        }
                    }
                }
                Err(_) => {}
            }
            if Instant::now() > deadline {
                return Err(anyhow!("guest agent not reachable within {timeout:?}"));
            }
            std::thread::sleep(Duration::from_millis(1));
        };
        stream.set_read_timeout(Some(
            deadline
                .saturating_duration_since(Instant::now())
                .max(Duration::from_millis(10)),
        ))?;
        stream.write_all(request.as_bytes())?;
        stream.write_all(b"\n")?;
        let mut line = String::new();
        BufReader::new(stream)
            .read_line(&mut line)
            .context("agent reply")?;
        let v: serde_json::Value = serde_json::from_str(&line).context("agent json")?;
        if v.get("ok").and_then(|b| b.as_bool()) != Some(true) {
            return Err(anyhow!(
                "agent error: {}",
                v.get("error").cloned().unwrap_or_default()
            ));
        }
        Ok(v.get("result").cloned().unwrap_or_default())
    }

    fn rss_kib(&self) -> u64 {
        std::fs::read_to_string(format!("/proc/{}/status", self.child.id()))
            .ok()
            .and_then(|s| {
                s.lines()
                    .find(|l| l.starts_with("VmRSS:"))
                    .and_then(|l| l.split_whitespace().nth(1))
                    .and_then(|n| n.parse().ok())
            })
            .unwrap_or(0)
    }
}

impl Drop for Vm {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.workdir);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn boot_args_use_read_only_root_and_our_init() {
        assert!(BOOT_ARGS.contains("root=/dev/vda ro"));
        assert!(BOOT_ARGS.contains("init=/sbin/rockbox-init"));
        assert!(BOOT_ARGS.contains("8250.nr_uarts=0"));
    }

    #[test]
    fn unavailable_without_kvm_or_assets() {
        // On the dev Mac / Docker Desktop there is no /dev/kvm; on a KVM host
        // without assets the message names the missing file.
        let r = unavailable_reason();
        if !Path::new("/dev/kvm").exists() {
            assert_eq!(r.as_deref(), Some("/dev/kvm not present"));
        }
    }
}
