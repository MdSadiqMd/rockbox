# /dev-min template

`/dev-min/` is a read-only directory the sandbox bind-mounts onto `/dev` inside each child

`mknod` of char devices is rejected inside an unprivileged user namespace, so we cannot create `/dev/null` etc from within the sandbox. Instead, a deployment-time script builds this directory on the host and the launcher bind-mounts it read-only

## Build

Run on the host as root (it uses `mknod`):

```bash
sudo scripts/build-dev-min.sh /var/lib/sandbox/dev-min
```

Contents:

| Path                    | Major | Minor | Mode |
|-------------------------|-------|-------|------|
| /dev-min/null           | 1     | 3     | 0666 |
| /dev-min/zero           | 1     | 5     | 0666 |
| /dev-min/full           | 1     | 7     | 0666 |
| /dev-min/urandom        | 1     | 9     | 0444 |
| /dev-min/random         | 1     | 8     | 0444 |
| /dev-min/tty            | 5     | 0     | 0666 |

GPU-enabled deployments build an additional `/dev-min-gpu/` containing `nvidia0`, `nvidiactl`, `nvidia-uvm`, `nvidia-uvm-tools`, and a `dri/` subdirectory. The launcher chooses the template based on the `+gpu` capability
