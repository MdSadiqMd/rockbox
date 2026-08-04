# Rockbox

Elixir orchestrator + Rust execution engine for running arbitrary code (and RL environments) inside a 10-layer Linux sandbox

> Native execution. No WASM, no VMs. Sandbox launch overhead is ~2-6 ms;
> measured end-to-end (Linux, release engine): ~13 ms p50 Python, ~62 ms
> TypeScript, ~90-370 ms compiled (per-request compile), ~36 ms/step RL

## Architecture 

```mermaid
flowchart TB
    subgraph Client["CLIENT"]
        C1([REST / WebSocket<br/>POST /api/execute])
    end

    subgraph Elixir["ELIXIR ORCHESTRATOR · Phoenix + OTP"]
        direction TB
        E1[Phoenix API<br/>REST + WS Channels]
        E2[Settings Pipeline<br/>14 steps → Effective]
        E3[Pool Manager<br/>hot/cold/autoscale]
        E4[VM.Server GenServer<br/>wraps Erlang Port]
        E5[PubSub<br/>vm:id broadcasts]
    end

    subgraph Rust["RUST ENGINE · per-VM OS process"]
        direction TB
        R1[App::run loop<br/>msgpack commands]
        R2[Mode Dispatch<br/>exec / session / rl]
        R3[Resolver<br/>settings → ChildSpec]
        R4[SandboxLauncher<br/>10-layer orchestration]
    end

    subgraph Sandbox["LINUX SANDBOX · 10 layers"]
        direction TB
        S1[clone3 + namespaces<br/>USER·PID·MNT·NET·IPC·UTS]
        S2[pivot_root<br/>/nix/store RO · /sandbox RO]
        S3[Seccomp BPF<br/>per-language profile]
        S4[AppArmor<br/>sandbox-executor]
        S5[Cgroup v2<br/>memory · cpu · pids]
        S6[Caps + RLIMIT<br/>drop all · set bounds]
        S7[execve<br/>user code runs native]
    end

    subgraph IO["I/O DRAIN"]
        direction LR
        IO1[io_uring / epoll<br/>stdout + stderr + pidfd]
        IO2[cgroup.kill<br/>atomic teardown]
    end

    C1 --> E1
    E1 --> E2 --> E3 --> E4
    E4 <-->|msgpack 4-byte framed| R1
    E4 <-.->|SOCK_SEQPACKET| IO1
    R1 --> R2 --> R3 --> R4
    R4 --> S1 --> S2 --> S3 --> S4 --> S5 --> S6 --> S7
    S7 --> IO1
    IO1 --> IO2
    IO2 -->|Result| R1
    R1 -->|Response| E4
    E4 --> E5 -->|push| C1
```

## Protocol Flow: Request Lifecycle

```mermaid
sequenceDiagram
    autonumber
    participant Client as Client<br/>curl / SDK
    participant Phoenix as Phoenix API<br/>Elixir
    participant Pipeline as Settings Pipeline<br/>14 steps
    participant Pool as Pool Manager<br/>GenServer
    participant VM as VM.Server<br/>GenServer
    participant Port as Erlang Port<br/>stdin/stdout
    participant Engine as Rust Engine<br/>per-VM process
    participant Launcher as SandboxLauncher<br/>kernel crate
    participant Child as Sandboxed Child<br/>user code

    rect rgb(30, 50, 80)
        Note over Client,Pipeline: INGRESS + SETTINGS
        Client->>+Phoenix: POST /api/execute<br/>{settings: {...}}
        Phoenix->>Phoenix: Authenticate (workspace + tier)
        Phoenix->>+Pipeline: run(payload, ctx)
        Pipeline->>Pipeline: 1. Schema validate
        Pipeline->>Pipeline: 2. Ensure request_id
        Pipeline->>Pipeline: 3. Ensure runtime
        Pipeline->>Pipeline: 4. Merge defaults<br/>(builtin → runtime → workspace)
        Pipeline->>Pipeline: 5. Clamp to tier ceilings
        Pipeline->>Pipeline: 6. Cross-field validate
        Pipeline->>Pipeline: 7. Resolve secrets
        Pipeline->>Pipeline: 8. Reserve quota
        Pipeline-->>-Phoenix: {:ok, %Effective{}}
    end

    rect rgb(50, 60, 50)
        Note over Pool,Port: VM ACQUISITION
        Phoenix->>+Pool: acquire(effective)
        Pool->>Pool: Check hot pool (ETS)
        alt Hot hit
            Pool-->>Phoenix: {:ok, vm_id}
        else Cold spawn
            Pool->>Pool: VM.Supervisor.start_vm
            Pool-->>Phoenix: {:ok, vm_id}
        end
        Pool-->>-Phoenix: vm_id
        Phoenix->>+VM: execute(vm_id, effective)
        VM->>+Port: Port.command(msgpack)
    end

    rect rgb(60, 50, 50)
        Note over Engine,Child: RUST ENGINE EXECUTION
        Port->>+Engine: Command::Execute(settings)
        Engine->>Engine: Resolver: settings → ChildSpec
        Engine->>Engine: Resolve runtime env (cache)
        alt Compiled language (Rust/C++)
            Engine->>Engine: Check binary cache
            Engine->>Engine: Compile if miss (bwrap)
        end
        Engine->>+Launcher: launch(ChildSpec)
        Launcher->>Launcher: Create cgroup
        Launcher->>Launcher: Pre-create pipes
        Launcher->>Launcher: clone3(NEWUSER|NEWPID|...)
        Launcher->>Launcher: Parent: write uid_map
        Launcher->>Launcher: Parent: add_pid(cgroup)
        Launcher->>+Child: Child: setup layers
        Child->>Child: 1. pivot_root
        Child->>Child: 2. AppArmor change_onexec
        Child->>Child: 3. NO_NEW_PRIVS
        Child->>Child: 4. Seccomp load BPF
        Child->>Child: 5. Drop capabilities
        Child->>Child: 6. Set RLIMIT
        Child->>Child: 7. execve(user code)
        Child->>Child: USER CODE RUNS NATIVE
    end

    rect rgb(50, 50, 70)
        Note over Engine,Child: DRAIN + RESULT
        Child-->>Launcher: stdout/stderr via pipes
        Child-->>Launcher: exit (or timeout → cgroup.kill)
        Launcher->>Launcher: io_uring drain
        Launcher->>Launcher: waitid(P_PIDFD)
        Launcher->>Launcher: Reset cgroup → pool
        Launcher-->>-Engine: ChildExit{status, output}
        Engine-->>-Port: Response::Result{...}
        Port-->>-VM: {:data, msgpack}
        VM->>VM: Decode + PubSub broadcast
        VM-->>-Phoenix: {:result, %{...}}
        Phoenix-->>-Client: JSON response
    end
```

## Elixir ↔ Rust Communication

```mermaid
sequenceDiagram
    autonumber
    participant EVM as BEAM VM<br/>Elixir
    participant GS as VM.Server<br/>GenServer
    participant Port as Erlang Port<br/>stdin/stdout
    participant Rust as Rust Engine<br/>tokio runtime
    participant DC as Data Channel<br/>SOCK_SEQPACKET

    Note over EVM,Rust: TWO CHANNELS: Control (Port) + Data (Unix socket)

    rect rgb(40, 50, 70)
        Note over GS,Rust: CONTROL CHANNEL · msgpack 4-byte framed
        GS->>Port: Port.open({:spawn_executable, engine_bin})
        Port->>+Rust: stdin: empty (boot)
        Rust-->>-Port: stdout: Response::Ready{pid, ...}
        Port-->>GS: {:data, msgpack}
        GS->>GS: Decode via Msgpax
        Note over GS,Rust: Commands: Execute, ExecCell, RlStep, Stdin, Interrupt, Lsp, Shutdown
        Note over GS,Rust: Responses: Ready, Result, CellResult, RlStep, EngineDied, Metrics
    end

    rect rgb(50, 60, 50)
        Note over GS,DC: DATA CHANNEL · streaming stdout/stderr
        GS->>DC: :gen_udp.open (SOCK_DGRAM listener)
        DC->>Rust: Engine connects on boot
        Note over DC,Rust: Batched 4ms / 4KB whichever first
        Rust-->>DC: <<1, len::32, stdout_bytes::binary>>
        DC-->>GS: {:udp, sock, ...}
        GS->>GS: PubSub.broadcast({:stdout, bytes})
        Rust-->>DC: <<2, len::32, stderr_bytes::binary>>
        DC-->>GS: PubSub.broadcast({:stderr, bytes})
    end

    rect rgb(60, 50, 50)
        Note over GS,Rust: ENGINE CRASH HANDLING
        Port-->>GS: {:exit_status, code}
        GS->>GS: PubSub.broadcast({:engine_died, code})
        GS->>GS: {:stop, {:engine_exit, code}}
    end
```

## 10-Layer Linux Sandbox

```mermaid
sequenceDiagram
    autonumber
    participant Parent as Parent Process<br/>Rust Engine
    participant Clone as clone3 syscall<br/>kernel
    participant Child as Child Process<br/>pre-exec
    participant Mounts as Mount NS<br/>pivot_root
    participant Sec as Security Layers<br/>AppArmor+Seccomp
    participant Exec as execve<br/>user code

    rect rgb(30, 50, 80)
        Note over Parent,Clone: LAYER 1-2: NAMESPACE ISOLATION
        Parent->>Parent: 1. Create cgroup (memory.max, cpu.max, pids.max)
        Parent->>Parent: 2. Pre-create pipes (stdout, stderr, sync)
        Parent->>Parent: 3. Compile seccomp BPF (cached)
        Parent->>+Clone: clone3(NEWUSER|NEWPID|NEWNS|NEWNET|NEWIPC|NEWUTS|PIDFD)
        Clone-->>Parent: child_pid + pidfd
        Clone-->>-Child: pid=0 in child
        Note over Parent,Child: Parent and Child now in different namespaces
    end

    rect rgb(40, 60, 50)
        Note over Parent,Child: LAYER 3: USER NAMESPACE SETUP
        Child->>Child: Block on sync pipe (wait for parent)
        Parent->>Parent: write(/proc/child/uid_map, "0 65534 1")
        Parent->>Parent: write(/proc/child/setgroups, "deny")
        Parent->>Parent: write(/proc/child/gid_map, "0 65534 1")
        Parent->>Parent: cgroup.add_pid(child_pid)
        Parent->>Child: Release sync pipe (write "x")
        Child->>Child: setresgid(0,0,0) + setresuid(0,0,0)
    end

    rect rgb(50, 70, 50)
        Note over Child,Mounts: LAYER 4: MOUNT NAMESPACE + pivot_root
        Child->>+Mounts: mount(tmpfs, /tmp/rockbox-newroot)
        Mounts->>Mounts: bind_mount(/nix/store, newroot/nix/store, RO)
        Mounts->>Mounts: bind_mount(/var/lib/sandbox/dev-min, newroot/dev, RO)
        Mounts->>Mounts: mount(proc, newroot/proc, hidepid=2)
        Mounts->>Mounts: mount(tmpfs, newroot/tmp, size=64M)
        Mounts->>Mounts: bind_mount(workdir, newroot/sandbox, RO)
        Mounts->>Mounts: pivot_root(newroot, newroot/old_root)
        Mounts->>Mounts: umount2(/old_root, MNT_DETACH)
        Mounts-->>-Child: FS isolated: only /nix/store, /dev, /proc, /tmp, /sandbox visible
    end

    rect rgb(60, 50, 50)
        Note over Child,Sec: LAYERS 5-8: SECURITY HARDENING
        Child->>+Sec: 5. AppArmor: aa_change_onexec("sandbox-executor")
        Sec->>Sec: 6. prctl(PR_SET_NO_NEW_PRIVS, 1) — BEFORE seccomp!
        Sec->>Sec: 7. Seccomp: load pre-compiled BPF filter
        Note over Sec: Profile: interp-aot (Python) / interp-jit (Node) / native-jit (Rust)
        Sec->>Sec: 8. Drop ALL 41 capabilities
        Sec->>Sec: 9. RLIMIT: FSIZE=50MB, NPROC, NOFILE=256, STACK=8MB, CORE=0
        Sec-->>-Child: Fully locked down
    end

    rect rgb(70, 50, 70)
        Note over Child,Exec: LAYER 10: EXECUTION
        Child->>+Exec: dup2(pipes → STDOUT/STDERR)
        alt Interpreted (Python/TS)
            Exec->>Exec: execve(/nix/.../python3, ["/sandbox/main.py"])
        else Compiled (Rust/C++)
            Exec->>Exec: execve(/sandbox/main, [])
        else Go
            Exec->>Exec: execve(/nix/.../go, ["run", "/sandbox/main.go"])
        end
        Note over Exec: USER CODE NOW RUNNING AT NATIVE SPEED
        Note over Exec: All 10 layers enforced by kernel — zero overhead
        Exec-->>-Parent: stdout/stderr via pipes, exit via pidfd
    end
```

## Session Mode (REPL/Notebook)

```mermaid
sequenceDiagram
    autonumber
    participant Client as Client
    participant API as SessionController
    participant Router as SessionRouter<br/>consistent hash
    participant Pool as Pool Manager
    participant VM as VM.Server
    participant Engine as Rust Engine
    participant Session as Session Runner<br/>/session/uuid volume

    rect rgb(30, 50, 80)
        Note over Client,Session: SESSION START
        Client->>+API: POST /api/sessions<br/>{settings: {mode: "session", ...}}
        API->>+Router: route(session_id)
        Router->>Router: Consistent hash → shard
        Router->>+Pool: acquire(effective)
        Pool-->>-Router: {:ok, vm_id}
        Router->>Router: Register session_id → vm_id
        Router-->>-API: vm_id
        API->>+VM: execute(vm_id, effective)
        VM->>+Engine: Command::Execute
        Engine->>Engine: Create /session/uuid volume (RW)
        Engine->>Engine: Init language runtime
        Engine->>Engine: Load Python/TS shim (globals persist)
        Engine-->>-VM: Response::Ready
        VM-->>-API: {:ok, session started}
        API-->>-Client: {session_id, vm_id}
    end

    rect rgb(50, 60, 50)
        Note over Client,Session: CELL EXECUTION (repeatable)
        Client->>+API: POST /api/sessions/:id/execute<br/>{code: "x = 42"}
        API->>+VM: exec_cell(vm_id, cell)
        VM->>+Engine: Command::ExecCell{id, code}
        Engine->>+Session: Run cell in persistent interpreter
        Session->>Session: exec(code) in globals dict
        Session->>Session: Pickle globals → /session/uuid/state.pkl
        Session-->>-Engine: {output, display_data}
        Engine-->>-VM: Response::CellResult
        VM-->>-API: cell result
        API-->>-Client: {output, display_data}
    end

    rect rgb(60, 50, 50)
        Note over Client,Session: SESSION RESTORE (after VM recycle)
        Client->>+API: POST /api/sessions/:id/execute
        API->>Router: route(session_id)
        Router->>Router: Lookup miss (VM recycled)
        Router->>+Pool: acquire(effective)
        Pool-->>-Router: {:ok, new_vm_id}
        Router->>+VM: execute(new_vm_id, effective)
        VM->>+Engine: Command::Execute
        Engine->>+Session: Check /session/uuid/state.pkl
        Session->>Session: unpickle → restore globals
        Session-->>-Engine: Globals restored
        Engine-->>-VM: Ready (state recovered)
        VM-->>-Router: ok
        Router-->>API: new_vm_id
        Note over API,Session: Continues as normal
    end
```

## RL Mode (Reinforcement Learning)

```mermaid
sequenceDiagram
    autonumber
    participant Agent as RL Agent<br/>training loop
    participant API as RLController
    participant Pool as Pool Manager
    participant VM as VM.Server
    participant Engine as Rust Engine
    participant RL as RL Runner<br/>/episode/uuid volume
    participant Env as User Env<br/>gridworld.py

    rect rgb(30, 50, 80)
        Note over Agent,Env: EPISODE START (reset)
        Agent->>+API: POST /api/rl/episodes<br/>{settings: {mode: "rl_step", ...}}
        API->>+Pool: acquire(effective)
        Pool-->>-API: {:ok, vm_id}
        API->>+VM: execute(vm_id, effective)
        VM->>+Engine: Command::Execute
        Engine->>Engine: Create /episode/uuid volume (RW)
        Engine->>+RL: Load user env module
        RL->>+Env: import gridworld<br/>obs = reset()
        Env-->>-RL: initial observation
        RL-->>-Engine: {obs, info}
        Engine-->>-VM: Response::Result{initial}
        VM-->>-API: {episode_id, vm_id, initial}
        API-->>-Agent: episode started
    end

    rect rgb(50, 60, 50)
        Note over Agent,Env: STEP LOOP (measured ~19-44 ms/step)
        loop Until done or max_steps
            Agent->>Agent: action = policy(obs)
            Agent->>+API: POST /api/rl/episodes/:id/step<br/>{action: base64(bytes)}
            API->>+VM: rl_step(vm_id, episode_id, action)
            VM->>+Engine: Command::RlStep{action}
            Engine->>+RL: step(action_bytes)
            RL->>+Env: obs, reward, done, info = step(action)
            Env-->>-RL: (obs, reward, done, info)
            RL->>RL: Checkpoint to /episode/uuid if needed
            RL-->>-Engine: {obs, reward, done, info}
            Engine-->>-VM: Response::RlStep
            VM-->>-API: step result
            API-->>-Agent: {obs, reward, done, info}
        end
    end

    rect rgb(60, 50, 50)
        Note over Agent,Env: EPISODE END
        Agent->>+API: DELETE /api/rl/episodes/:id
        API->>+VM: Supervisor.stop_vm
        VM->>+Engine: terminate
        Engine->>Engine: Cleanup /episode/uuid
        Engine-->>-VM: exit
        VM-->>-API: ok
        API-->>-Agent: {ok: true}
    end
```

## Pool Management + Autoscaling

```mermaid
sequenceDiagram
    autonumber
    participant Req as Incoming Request
    participant Pool as Pool.Manager<br/>GenServer
    participant ETS as ETS Table<br/>:rockbox_pool
    participant Quota as QuotaTracker
    participant Sup as VM.Supervisor<br/>DynamicSupervisor
    participant Auto as Autoscaler<br/>GenServer

    rect rgb(30, 50, 80)
        Note over Req,Sup: ACQUIRE PATH
        Req->>+Pool: acquire(effective)
        Pool->>+Quota: reserve(workspace_id)
        alt Quota OK
            Quota-->>Pool: :ok
            Pool->>+ETS: take_from_pool(key)
            alt Hot hit
                ETS-->>Pool: {:ok, vm_id}
                Pool->>ETS: mark_busy(vm_id)
                Pool-->>-Req: {:ok, vm_id}
            else Pool empty
                ETS-->>Pool: :empty
                Pool->>+Sup: start_vm(settings)
                Sup->>Sup: VM.Server.start_link
                Sup-->>-Pool: {:ok, vm_id}
                Pool->>ETS: mark_busy(vm_id)
                Pool-->>Req: {:ok, vm_id}
            end
        else Quota exceeded
            Quota-->>-Pool: {:error, :concurrency_exceeded}
            Pool-->>Req: {:error, :concurrency_exceeded}
        end
    end

    rect rgb(50, 60, 50)
        Note over Req,ETS: RELEASE PATH
        Req->>+Pool: release(vm_id, settings)
        alt mode == :exec (one-shot)
            Pool->>+Sup: stop_vm(vm_id)
            Sup-->>-Pool: ok
            Pool->>ETS: delete(vm_id)
            Pool->>Quota: release(workspace_id)
        else mode == :session or :rl (reusable)
            Pool->>ETS: mark_idle(vm_id, timestamp)
            Note over ETS: VM returns to hot pool for reuse
        end
        Pool-->>-Req: ok
    end

    rect rgb(60, 50, 50)
        Note over Auto,Sup: AUTOSCALER (background, EWMA-based)
        loop Every 5s
            Auto->>+ETS: snapshot()
            ETS-->>-Auto: [{vm_id, key, status, ts}, ...]
            Auto->>Auto: Compute utilization per (workspace, language)
            Auto->>Auto: EWMA smoothing (α=0.3)
            alt util > 0.8 (scale up)
                Auto->>+Sup: start_vm (pre-warm)
                Sup-->>-Auto: ok
                Auto->>ETS: insert(vm_id, :idle)
            else util < 0.3 (scale down)
                Auto->>+Pool: retire(key, count)
                Pool->>Pool: Select oldest idle VMs
                Pool->>Sup: stop_vm (FIFO order)
                Pool->>ETS: delete
                Pool-->>-Auto: ok
            end
        end
    end
```

## Master End-to-End Sequence

```mermaid
sequenceDiagram
    autonumber
    participant Client as Client<br/>REST/WebSocket
    participant Phoenix as Phoenix API<br/>Auth + Rate Limit
    participant Pipeline as Settings Pipeline<br/>14 steps
    participant Quota as QuotaTracker<br/>per-workspace
    participant Pool as Pool.Manager<br/>hot/cold
    participant VM as VM.Server<br/>GenServer
    participant Port as Erlang Port<br/>stdin/stdout
    participant DC as Data Channel<br/>SOCK_SEQPACKET
    participant Engine as Rust Engine<br/>App::run
    participant EnvCache as Env Cache<br/>memfd T1
    participant BinCache as Binary Cache<br/>verified fd T2
    participant Launcher as SandboxLauncher<br/>kernel crate
    participant Cgroup as Cgroup v2<br/>memory+cpu+pids
    participant Child as Sandboxed Child<br/>10 layers
    participant NixStore as /nix/store<br/>RO T3
    participant SessionVol as /session/<br/>RW T4
    participant EpisodeVol as /episode/<br/>RW T5
    participant Audit as AuditLog<br/>Postgres
    participant Cost as CostTracker<br/>250ms tick
    participant Webhook as WebhookDispatcher<br/>HMAC+DLQ

    rect rgb(25, 40, 70)
        Note over Client,Pipeline: ① INGRESS + SETTINGS PIPELINE
        Client->>+Phoenix: POST /api/execute {settings}
        Phoenix->>Phoenix: Authenticate workspace + tier
        Phoenix->>Phoenix: Rate limit check
        Phoenix->>+Pipeline: run(payload, ctx)
        Pipeline->>Pipeline: 1. Schema validate (v1+)
        Pipeline->>Pipeline: 2. Ensure request_id
        Pipeline->>Pipeline: 3. Ensure runtime (python-base, ts-modern, ...)
        Pipeline->>Pipeline: 4. Merge defaults (builtin→runtime→workspace)
        Pipeline->>Pipeline: 5. Clamp to tier ceilings (free/pro/enterprise)
        Pipeline->>Pipeline: 6. Cross-field validate (conflict table)
        Pipeline->>Pipeline: 7. Resolve secrets (vault refs → env)
        Pipeline->>+Quota: 8. Reserve quota
        Quota-->>-Pipeline: :ok
        Pipeline->>Pipeline: 9. FREEZE → %Effective{} immutable
        Pipeline->>+Audit: Write requested + effective + clamped
        Audit-->>-Pipeline: ok
        Pipeline-->>-Phoenix: {:ok, %Effective{}}
    end

    rect rgb(40, 55, 45)
        Note over Pool,Port: ② VM ACQUISITION + PORT SETUP
        Phoenix->>+Pool: acquire(effective)
        Pool->>Pool: key = {workspace, language, runtime}
        Pool->>Pool: Check ETS hot pool
        alt Hot hit (O(1))
            Pool->>Pool: mark_busy(vm_id)
        else Cold spawn
            Pool->>Pool: VM.Supervisor.start_vm
            Pool->>Pool: insert ETS, mark_busy
        end
        Pool-->>-Phoenix: {:ok, vm_id}
        Phoenix->>+VM: execute(vm_id, effective)
        VM->>+Port: Port.open({:spawn_executable, engine_bin})
        Port->>+Engine: stdin: boot
        Engine->>Engine: tracing_subscriber init (stderr)
        Engine-->>-Port: stdout: Response::Ready{pid, schema}
        Port-->>VM: {:data, msgpack}
        VM->>+DC: :gen_udp.open (SOCK_DGRAM)
        DC-->>-VM: listener ready
        Note over VM,DC: Two channels: Control (Port) + Data (DC)
    end

    rect rgb(55, 45, 40)
        Note over Engine,BinCache: ③ RUST ENGINE DISPATCH + RESOLUTION
        VM->>+Port: Port.command(msgpack Execute)
        Port->>+Engine: Command::Execute(settings)
        Engine->>Engine: state.set_language(settings.language)
        Engine->>Engine: Mode dispatch: exec / session / rl
        Engine->>+EnvCache: Lookup env_hash = sha256(flake.lock+lang+arch)
        alt Cache hit (memfd, ~0µs)
            EnvCache-->>Engine: env_vars from mmap
        else Cache miss (~2-10s)
            EnvCache->>EnvCache: nix print-dev-env → serialize
            EnvCache-->>Engine: env_vars
        end
        EnvCache-->>-Engine: PATH, PYTHONHOME, GOROOT, ...
        alt Compiled language (Rust/C++)
            Engine->>+BinCache: Lookup code_hash = sha256(files+compiler+flake.lock)
            alt Binary cache hit
                BinCache-->>Engine: verified_fd (O_RDONLY|O_NOFOLLOW)
            else Binary cache miss
                BinCache->>BinCache: bwrap compile (network-isolated)
                BinCache->>BinCache: sha256(binary) → .sha256, chmod 555
                BinCache-->>Engine: verified_fd
            end
            BinCache-->>-Engine: binary_fd_path = /proc/self/fd/N
        end
        Engine->>Engine: Build ChildSpec{argv, env, mounts, limits, seccomp}
    end

    rect rgb(50, 40, 55)
        Note over Launcher,Child: ④ SANDBOX LAUNCH (10 LAYERS)
        Engine->>+Launcher: launch(ChildSpec)
        Launcher->>+Cgroup: Create rockbox-{request_id}
        Cgroup->>Cgroup: memory.max, cpu.max, pids.max
        Cgroup-->>-Launcher: cgroup_fd
        Launcher->>Launcher: Pre-create pipes (stdout, stderr, sync)
        Launcher->>Launcher: Compile seccomp BPF (cached per profile)
        Launcher->>Launcher: clone3(NEWUSER|NEWPID|NEWNS|NEWNET|NEWIPC|NEWUTS|PIDFD)
        Note over Launcher,Child: Parent and Child fork here
        
        par Parent (Rust Engine)
            Launcher->>Launcher: write(/proc/child/uid_map, "0 65534 1")
            Launcher->>Launcher: write(/proc/child/setgroups, "deny")
            Launcher->>Launcher: write(/proc/child/gid_map, "0 65534 1")
            Launcher->>Cgroup: cgroup.procs ← child_pid
            Launcher->>Launcher: Release sync pipe (write "x")
        and Child (pre-exec)
            Child->>Child: Block on sync pipe
            Child->>Child: setresgid(0,0,0) + setresuid(0,0,0)
            Child->>Child: L3: Mount NS setup
            Child->>+NixStore: bind_mount(/nix/store, RO)
            NixStore-->>-Child: /nix/store visible
            Child->>Child: bind_mount(/dev-min, RO)
            Child->>Child: mount(proc, hidepid=2)
            Child->>Child: mount(tmpfs /tmp, 64MB)
            Child->>Child: bind_mount(workdir → /sandbox, RO)
            alt Session mode
                Child->>+SessionVol: bind_mount(/session/uuid, RW)
                SessionVol-->>-Child: persistent state volume
            else RL mode
                Child->>+EpisodeVol: bind_mount(/episode/uuid, RW)
                EpisodeVol-->>-Child: replay buffer volume
            end
            Child->>Child: pivot_root + umount2(/old_root)
            Child->>Child: L0/8: AppArmor aa_change_onexec("sandbox-executor")
            Child->>Child: L7: prctl(PR_SET_NO_NEW_PRIVS, 1)
            Child->>Child: L3: Seccomp load BPF (interp-aot/jit, native-jit, go, rl-step)
            Child->>Child: L5: Drop ALL 41 capabilities
            Child->>Child: L6: RLIMIT (FSIZE=50MB, NPROC, NOFILE=256, STACK=8MB)
            Child->>Child: dup2(pipes → STDOUT/STDERR)
            Child->>Child: L10: execve(interpreter or binary)
            Note over Child: USER CODE NOW RUNNING NATIVE
        end
    end

    rect rgb(45, 50, 60)
        Note over Engine,Webhook: ⑤ EXECUTION + DRAIN + RESULT
        loop User code running
            Child-->>DC: stdout chunks (batched 4ms/4KB)
            DC-->>VM: {:udp, ...}
            VM->>VM: PubSub.broadcast({:stdout, bytes})
            VM-->>Phoenix: push to channel
            Phoenix-->>Client: WSS stream
        end
        
        par CostTracker monitoring
            Cost->>Cost: 250ms tick
            Cost->>Cgroup: Read memory.current, cpu.stat
            alt cost_budget exceeded
                Cost->>Cgroup: cgroup.kill = 1
            end
        end

        alt Normal exit
            Child-->>Launcher: exit(code)
        else Timeout
            Launcher->>Cgroup: cgroup.kill = 1 (atomic SIGKILL all)
        else Output cap exceeded
            Launcher->>Cgroup: cgroup.kill = 1
        end
        
        Launcher->>Launcher: io_uring drain (stdout + stderr + pidfd)
        Launcher->>Launcher: waitid(P_PIDFD) → real exit code
        Launcher->>Cgroup: wait cgroup.events:populated=0
        Launcher->>Cgroup: Reset memory.peak, pids.peak
        Launcher->>Pool: Return cgroup to pool
        Launcher-->>-Engine: ChildExit{status, output, errors, exec_time_ms}
        
        Engine->>Engine: Build Response::Result
        Engine-->>-Port: stdout: msgpack Response::Result
        Port-->>-VM: {:data, bytes}
        VM->>VM: Msgpax.unpack! → decode
        VM->>VM: PubSub.broadcast({:result, msg})
        VM->>+Audit: Write outcome (status, exit_code, exec_time, memory)
        Audit-->>-VM: ok
        VM->>+Webhook: emit("completed", result) HMAC-signed
        Webhook->>Webhook: 3× exp-backoff, DLQ on failure
        Webhook-->>-VM: ok
        VM-->>-Phoenix: {:result, %{...}}
        Phoenix->>+Pool: release(vm_id, settings)
        alt mode == :exec
            Pool->>Pool: VM.Supervisor.stop_vm
            Pool->>Quota: release(workspace_id)
        else mode == :session or :rl
            Pool->>Pool: mark_idle (return to hot pool)
        end
        Pool-->>-Phoenix: ok
        Phoenix-->>-Client: JSON {status, output, errors, exec_time_ms, memory_peak_mb}
    end
```

## Quickstart (dev)

Requires Erlang 26+, Elixir 1.18+, Postgres 15+, Rust 1.85+ (edition 2024).

```bash
# 1. Fetch deps + set up the dev DB
make setup

# 2. Build the Rust engine (release mode)
make engine

# 3. Start the Phoenix server
make server
```

On macOS the Rust engine builds and runs but **does not sandbox** (Linux-only primitives are cfg-gated to no-op stubs). Use a Linux box / VM / container for end-to-end testing of the sandbox itself

## Try it

```bash
curl -X POST http://localhost:4000/api/execute \
  -H 'authorization: Bearer token-ws_pro_demo-pro' \
  -H 'content-type: application/json' \
  -d '{
    "settings": {
      "language": "python",
      "runtime":  "python-base",
      "files":    [{"path":"main.py", "content":"print(2+2)"}],
      "limits":   {"wall_ms": 5000, "memory_mb": 256}
    }
  }'
```

## Benchmarks vs Alternatives (2026)

Rockbox compared against leading AI code execution sandboxes. Data sourced from
third-party benchmarks and official documentation — see sources below.

### Cold Start Latency

| Platform   | Cold Start | Isolation Model | Notes |
|------------|------------|-----------------|-------|
| **Rockbox** | **46-72 ms** (interpreted) / **82-351 ms** (compiled, includes per-request compile) | Linux namespaces + seccomp + AppArmor + cgroups | Native execution, no VM overhead. Sandbox launch itself ≈2-6 ms; measured end-to-end, see methodology below |
| Daytona | 27-90 ms | gVisor containers | Fastest among commercial alternatives. Sub-90ms in production |
| E2B | 150-300 ms | Firecracker microVM | Hardware-level isolation, separate kernel per sandbox |
| Modal | 100-500 ms (CPU) / 1-2s (GPU) | gVisor containers | Optimized for GPU workloads with snapshotting |

#### Methodology (Rockbox numbers)

Measured July 2026 on the `docker compose` stack (Linux container, privileged)
with the **release** engine build (`ROCKBOX_ENGINE_BIN=core/target/release/engine`):
`curl → POST /api/execute` wall time for `print(2+2)` / `console.log(2+2)` /
trivial Go/Rust/C++ programs. Warm column: 10 requests per language (min-free
percentiles); cold column: first request after an app-service restart.
`exec_time_ms` is engine-internal (sandbox launch → drain); the gap to wall
time is HTTP ingress + settings pipeline (incl. the Postgres audit write) +
pool + port + engine process boot.

| Language | Cold (fresh app) | Warm p50 | Warm p95 | Engine exec p50 |
|----------|------------------|----------|----------|-----------------|
| Python | 46 ms | 13 ms | 33 ms | 9 ms |
| TypeScript | 72 ms | 62 ms | 66 ms | 58 ms |
| Go | 82 ms | 90 ms | 99 ms | 21 ms |
| Rust | 101 ms | 91 ms | 98 ms | 20 ms |
| C++ | 351 ms | 367 ms | 382 ms | 23 ms |

Notes:
- Interpreter boot dominates interpreted runs (CPython ~15-40 ms, Node
  ~20-80 ms); the ~2-6 ms figure is launch overhead only, not a full request.
- A debug engine build roughly doubles Python p50 (29 ms vs 13 ms); compiled
  languages are unaffected (compilers + interpreter boot dominate).
- Compiled languages recompile on every request — verified: the `cache`
  crate (`BinaryCache`/`EnvCache`) is instantiated in `EngineState`
  (`core/crates/engine/src/state.rs`) but never called, and the `compiler`
  compile-helper binary (which implements the `/var/cache/sandbox/bin`
  cache) is not started by the compose stack and has no socket client in the
  engine. Observed: `rustc` subprocess per request, no cache dir ever
  created, identical requests stay at ~90-100 ms (sandbox exec is only
  20-23 ms of that). Wiring the helper in should bring compiled requests
  down to the `exec_time_ms` column.
- `exec` mode stops the VM after each request, so every request pays a fresh
  engine-process boot; session/RL modes reuse the VM.

### RL Step Latency (Hot Path)

| Platform   | Per-Step Latency | Notes |
|------------|------------------|-------|
| **Rockbox** | **~19-44 ms** | Each step = HTTP + fresh sandbox spawn + interpreter boot + pickle state round-trip (see `core/crates/engine/src/modes/rl.rs`) |
| E2B | ~1-5 ms | Each tool call crosses network boundary |
| Modal | ~1-10 ms | Network round-trip per invocation |
| Daytona | ~1-5 ms | Stateful execution but still networked |

### Pricing (CPU Compute)

| Platform   | Cost | Billing Model |
|------------|------|---------------|
| **Rockbox** | **Self-hosted** | Your infrastructure costs only |
| E2B | $0.0504/vCPU-hr ($0.000014/s per vCPU) | Per-second, includes idle time |
| Daytona | $0.0504/vCPU-hr | Per-second, $200 free credit |
| Modal | $0.047/CPU-hr ($0.0000131/s) | Per-second, no idle charges. 3.75x multiplier for non-preemptible |

### Security Model

| Platform   | Isolation | Kernel Shared? | Escape Risk |
|------------|-----------|----------------|-------------|
| **Rockbox** | User NS + PID NS + Mount NS + Net NS + IPC NS + UTS NS + Seccomp BPF + AppArmor + Cgroups v2 + Capability drop | No (pivot_root to clean fs) | Low — 10 independent layers, kernel-enforced |
| E2B | Firecracker microVM | No (separate kernel) | Very Low — hardware boundary |
| Modal | gVisor user-space kernel | No (syscall interception) | Low — syscalls never reach host kernel |
| Daytona | gVisor containers | No (syscall interception) | Low — user-space kernel |

### Resource Limits

| Platform   | Max Memory | Max CPU | Max Execution Time |
|------------|------------|---------|-------------------|
| **Rockbox** | Configurable (default 256MB, up to 32GB) | Configurable (cgroup cpu.max) | Configurable (default 60s, RL episodes up to 24h) |
| E2B | Tier-dependent | 2+ vCPUs (Pro) | 1h (Hobby) / 24h (Pro) |
| Modal | Up to 256GB | Up to 32 cores | 24h max |
| Daytona | Configurable (Pro) | Configurable (Pro) | 15min idle timeout, configurable |

### Languages Supported

| Platform   | Languages |
|------------|-----------|
| **Rockbox** | Python, TypeScript, Go, Rust, C++ |
| E2B | Python, JavaScript/TypeScript, Bash, any via custom templates |
| Modal | Python (primary), any via containers |
| Daytona | Any (container-based dev environments) |

### Feature Comparison

| Feature | Rockbox | E2B | Modal | Daytona |
|---------|---------|-----|-------|---------|
| Self-hostable | ✅ | ❌ (SaaS only) | ❌ (SaaS only) | ✅ (open source) |
| Session/REPL mode | ✅ | ✅ | ✅ | ✅ |
| RL training mode | ✅ | ❌ | Partial (via containers) | ❌ |
| GPU passthrough | ✅ (opt-in) | ✅ (enterprise) | ✅ | ❌ |
| LSP relay | ✅ | ❌ | ❌ | ✅ |
| Network isolation | ✅ (4 tiers) | ✅ | ✅ | ✅ |
| Filesystem persistence | ✅ (/session, /episode volumes) | ✅ | ✅ (snapshots) | ✅ |
| Pre-baked runtimes | ✅ (Nix flakes) | ✅ (templates) | ✅ (images) | ✅ (devcontainers) |

### When to Use What

| Use Case | Recommended |
|----------|-------------|
| **Lowest latency, self-hosted** | Rockbox |
| **RL training (self-hosted)** | Rockbox |
| **Managed service, security-first** | E2B (Firecracker isolation) |
| **GPU inference at scale** | Modal |
| **Full dev environments** | Daytona |
| **Agent tool execution (managed)** | E2B or Daytona |

**Sources:**
- [E2B vs Daytona vs Blaxel Comparison 2026](https://baeseokjae.github.io/posts/e2b-vs-daytona-vs-blaxel-2026/)
- [AI Sandbox Pricing Comparison 2026](https://northflank.com/blog/ai-sandbox-pricing)
- [Modal Cold Start Performance](https://modal.com/docs/guide/cold-start)
- [Daytona vs E2B 2026](https://northflank.com/blog/daytona-vs-e2b-ai-code-execution-sandboxes)
- [Modal Pricing Explained 2026](https://www.beam.cloud/blog/modal-pricing-explained)
- [E2B Pricing Breakdown 2026](https://www.morphllm.com/e2b-pricing)
- [AI Code Sandboxes Security Study](https://arxiv.org/pdf/2606.08433)
- [Modal Best Sandbox Infrastructure 2026](https://modal.com/resources/best-sandbox-infrastructure-multi-tenant-ai-apps)

## License

BSD 3-Clause
