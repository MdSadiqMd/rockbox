# Rockbox — Runnable Samples

Small programs you can POST to `/api/execute` to exercise the system end-to-end.

Every sample is **self-contained** — the helper script (`scripts/run-sample.sh`)
detects the language from the file extension, picks the matching runtime, and
posts the JSON payload to a running stack.

> Bring the stack up first: `just docker-up`

## Quick demo

```bash
just demo-python              # priv/samples/python/hello.py
just demo-ts                  # priv/samples/typescript/hello.ts
just demo-go                  # priv/samples/go/hello.go
just demo-rust                # priv/samples/rust/hello.rs
just demo-cpp                 # priv/samples/cpp/hello.cpp
just demo-multi-file          # priv/samples/multi_file (main.py + greeter.py + math_utils.py)
just demo-all                 # one of each
```

For prettier output:

```bash
PRETTY=1 just demo-python
```

## Run any file directly

```bash
just demo priv/samples/python/concurrent.py
just demo priv/samples/go/goroutines.go
```

## Per-sample raw curl

If you'd rather paste a curl, the script prints the equivalent shape — these
are the canonical forms:

### Python (single file)

```bash
curl -X POST http://localhost:4000/api/execute \
  -H 'authorization: Bearer token-ws_pro_demo-pro' \
  -H 'content-type: application/json' \
  -d "$(jq -n \
    --rawfile code priv/samples/python/hello.py \
    '{settings: {language: "python", runtime: "python-base",
                 entrypoint: "hello.py",
                 files: [{path: "hello.py", content: $code}]}}')"
```

### Multi-file Python

```bash
files=$(jq -n \
  --rawfile main priv/samples/multi_file/main.py \
  --rawfile g priv/samples/multi_file/greeter.py \
  --rawfile m priv/samples/multi_file/math_utils.py \
  '[{path:"main.py",content:$main},
    {path:"greeter.py",content:$g},
    {path:"math_utils.py",content:$m}]')

curl -X POST http://localhost:4000/api/execute \
  -H 'authorization: Bearer token-ws_pro_demo-pro' \
  -H 'content-type: application/json' \
  -d "$(jq -n --argjson files "$files" \
    '{settings: {language: "python", runtime: "python-base",
                 entrypoint: "main.py", files: $files}}')"
```

### Go (compiled — wait for compile + run)

```bash
curl -X POST http://localhost:4000/api/execute \
  -H 'authorization: Bearer token-ws_pro_demo-pro' \
  -H 'content-type: application/json' \
  -d "$(jq -n \
    --rawfile code priv/samples/go/hello.go \
    '{settings: {language: "go", runtime: "go-std",
                 entrypoint: "hello.go",
                 files: [{path: "hello.go", content: $code}],
                 limits: {wall_ms: 30000, compile_ms: 30000}}}')"
```

## What's in here

```
priv/samples/
├── python/
│   ├── hello.py          # baseline
│   ├── fibonacci.py      # loops + ints
│   └── concurrent.py     # threading.Thread (needs +concurrency)
├── typescript/
│   ├── hello.ts          # baseline
│   └── async.ts          # Promise.all
├── go/
│   ├── hello.go
│   └── goroutines.go     # 100 goroutines + atomic
├── rust/
│   ├── hello.rs
│   └── threads.rs        # std::thread + Arc<Atomic>
├── cpp/
│   └── hello.cpp
├── multi_file/           # multi-file Python project
│   ├── main.py
│   ├── greeter.py
│   └── math_utils.py
└── session/
    └── init.py           # session-mode bootstrap cell
```

## Auth

The script defaults to `Authorization: Bearer token-ws_pro_demo-pro`. Use:

| Token                            | Workspace      | Tier      |
|----------------------------------|----------------|-----------|
| `token-ws_free_demo-free`        | `ws_free_demo` | free      |
| `token-ws_pro_demo-pro`          | `ws_pro_demo`  | pro       |

Override via `ROCKBOX_TOKEN=token-ws_…-… just demo-python`.
