set shell := ["bash", "-cu"]

CORE        := "core"
CARGO_FLAGS := "--manifest-path " + CORE + "/Cargo.toml"
ENGINE_BIN  := CORE + "/target/release/engine"
HELPER_BIN  := CORE + "/target/release/compiler"

default:
    @just --list

# Fetch Elixir deps, create + migrate the dev DB
setup:
    mix deps.get
    mix ecto.create
    mix ecto.migrate

# Release build of the engine + compiler binaries
engine:
    cargo build {{CARGO_FLAGS}} --release --workspace --bins
    @ls -lh {{ENGINE_BIN}} {{HELPER_BIN}}

# Debug build (fast)
engine-dev:
    cargo build {{CARGO_FLAGS}} --workspace --bins

# Watch + rebuild engine on every Rust change (for native dev)
engine-watch:
    cargo watch {{CARGO_FLAGS}} -- cargo build {{CARGO_FLAGS}} --workspace --bins

# Run Phoenix in foreground (auto code reload)
server:
    iex --sname rockbox -S mix phx.server

# Run Phoenix as a background daemon (logs to `.phx.log`)
server-bg:
    elixir --erl "-detached" -S mix phx.server > .phx.log 2>&1 &

# Run all tests (Rust + Elixir)
test: test-rust test-elixir

# Cargo unit tests across the workspace
test-rust:
    cargo test {{CARGO_FLAGS}} --workspace --lib

# Cargo doc tests
test-rust-doc:
    cargo test {{CARGO_FLAGS}} --workspace --doc

# Elixir tests (creates the test DB on first run)
test-elixir:
    mix test

# Property-based + slow tests, run sparingly
test-prop:
    cargo test {{CARGO_FLAGS}} --workspace --features proptest -- --ignored

# Format every language we touch
fmt: fmt-rust fmt-elixir fmt-nix

# Cargo fmt (every crate)
fmt-rust:
    cargo fmt {{CARGO_FLAGS}} --all

# Elixir formatter
fmt-elixir:
    mix format

# Nix formatter (skipped if `nixfmt` not installed)
fmt-nix:
    @if command -v nixfmt >/dev/null; then \
        find nix -name '*.nix' -print0 | xargs -0 nixfmt; \
    else \
        echo "skipping nix (install nixfmt: brew install nixfmt-rfc-style)"; \
    fi

# Verify all sources are formatted (CI gate)
fmt-check:
    cargo fmt {{CARGO_FLAGS}} --all -- --check
    mix format --check-formatted
    @if command -v nixfmt >/dev/null; then \
        find nix -name '*.nix' -print0 | xargs -0 nixfmt --check; \
    fi

# Rust + Elixir lints
lint: lint-rust lint-elixir

# Cargo clippy across the workspace
lint-rust:
    cargo clippy {{CARGO_FLAGS}} --workspace --all-targets -- -D warnings

# Credo style checks + Dialyzer type checking
lint-elixir:
    mix credo --strict
    mix dialyzer --no-check

# Security scan (Elixir)
audit:
    mix sobelow --skip
    cargo audit || true   # cargo-audit is optional


# Run pending database migrations
migrate:
    mix ecto.migrate

# Roll back the last database migration
migrate-rollback:
    mix ecto.rollback

# Create the dev database
db-create:
    mix ecto.create

# Drop the dev database
db-drop:
    mix ecto.drop

# Drop, recreate, migrate, and seed the dev database
db-reset:
    mix ecto.drop
    mix ecto.create
    mix ecto.migrate
    mix run priv/repo/seeds.exs

# Build the /dev-min template. Requires root + Linux
dev-min:
    sudo scripts/build-dev-min.sh

# Pre-evaluate every catalog flake
prewarm:
    scripts/prewarm-flakes.sh

# Build the dev image and bring everything up
docker-up:
    docker compose up -d --build
    @echo "Stack up. API: http://localhost:4000  ·  DB: localhost:5433"

# Tail logs from all services
docker-logs:
    docker compose logs -f --tail=200

# Open a bash shell inside the app container
docker-shell:
    docker compose exec app bash

# Run tests inside the running stack (uses container Postgres)
docker-test:
    docker compose exec app bash -lc "MIX_ENV=test mix test"
    docker compose exec app bash -lc "cargo test --manifest-path core/Cargo.toml --workspace --lib"

# Restart only the app (engine watcher keeps running)
docker-restart-app:
    docker compose restart app

# Stop the stack but keep volumes
docker-down:
    docker compose down

# Nuke volumes — wipes DB + caches. Useful when migrations get tangled
docker-nuke:
    docker compose down -v

# These are NOT meant to be run from the host directly; compose calls them as
# the container entrypoint. Keeping them here means the "what each service
# actually does" lives in version control alongside the host commands, not
# inlined in YAML.

# `app` service entrypoint: wait for Postgres + first engine build, then boot
docker-app-serve:
    @echo "waiting for postgres ($PGHOST:5432)…"
    @until pg_isready -h "${PGHOST:-postgres}" -U "${PGUSER:-postgres}" >/dev/null 2>&1; do sleep 1; done
    @echo "waiting for first engine build at {{CORE}}/target/debug/engine…"
    @until [ -f {{CORE}}/target/debug/engine ]; do sleep 2; done
    @echo "booting Phoenix"
    mix deps.get
    -mix ecto.create --quiet
    mix ecto.migrate
    elixir --sname rockbox -S mix phx.server

# `engine-watch` service entrypoint: rebuild engine + compiler on every change.
# cargo-watch resolves the project root from CWD, so we cd into `core/` first.
docker-engine-watch:
    cd {{CORE}} && cargo watch \
        --watch crates --watch Cargo.toml \
        --shell 'cargo build --workspace --bins && touch .build-stamp'

# All recipes POST to /api/execute. Override target / auth via:
#   ROCKBOX_URL=…   ROCKBOX_TOKEN=…   PRETTY=1 just demo-python
# `PRETTY=1` pipes the JSON through `jq` for readable output

# Run an arbitrary sample by path
demo path:
    scripts/run-sample.sh {{path}} | jq

# Python hello world
demo-python:
    scripts/run-sample.sh priv/samples/python/hello.py | jq

# Python threading demo
demo-python-threads:
    scripts/run-sample.sh priv/samples/python/concurrent.py | jq

# Python fibonacci demo
demo-python-fib:
    scripts/run-sample.sh priv/samples/python/fibonacci.py | jq

# TypeScript hello world
demo-ts:
    scripts/run-sample.sh priv/samples/typescript/hello.ts | jq

# TypeScript async/await demo
demo-ts-async:
    scripts/run-sample.sh priv/samples/typescript/async.ts | jq

# Go hello world
demo-go:
    scripts/run-sample.sh priv/samples/go/hello.go | jq

# Go goroutines demo
demo-go-routines:
    scripts/run-sample.sh priv/samples/go/goroutines.go | jq

# Rust hello world
demo-rust:
    scripts/run-sample.sh priv/samples/rust/hello.rs | jq

# Rust threads demo
demo-rust-threads:
    scripts/run-sample.sh priv/samples/rust/threads.rs | jq

# C++ hello world
demo-cpp:
    scripts/run-sample.sh priv/samples/cpp/hello.cpp | jq

# Multi-file project demo (Python)
demo-multi-file:
    scripts/run-sample.sh priv/samples/multi_file python main.py | jq

# End-to-end platform check — exercises both the Elixir API (lib/) and the Rust engine (core/)
demo-platform-check:
    scripts/run-sample.sh priv/samples/platform-check/main.py | jq

# Hit every language sample in sequence
demo-all:
    @just demo-python
    @just demo-python-threads
    @just demo-python-fib
    @just demo-ts
    @just demo-ts-async
    @just demo-go
    @just demo-go-routines
    @just demo-rust
    @just demo-rust-threads
    @just demo-cpp
    @just demo-multi-file

# Runs the gridworld RL sample: starts episode, N random steps, destroys
# Override steps: `just demo-rl 50`
demo-rl steps="10":
    scripts/run-rl-demo.sh {{steps}}

# Remove build artifacts (Elixir _build + deps, Rust target)
clean:
    rm -rf _build deps
    cargo clean {{CARGO_FLAGS}}

# Print a one-shot dev environment status report
doctor:
    @command -v mix    && mix --version    || echo "mix MISSING"
    @command -v cargo  && cargo --version  || echo "cargo MISSING"
    @command -v docker && docker --version || echo "docker MISSING"
    @command -v just   && just --version   || echo "just MISSING"
    @echo "engine bin : $(test -f {{ENGINE_BIN}} && echo ✓ || echo missing)"
    @echo "helper bin : $(test -f {{HELPER_BIN}} && echo ✓ || echo missing)"
    @echo "_build     : $(test -d _build && echo ✓ || echo missing)"
