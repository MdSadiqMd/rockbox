import Config

# Lean environment for latency benchmarking: dev-style bind + secrets, but
# with the dev-only per-request machinery stripped out (CodeReloader,
# debug logging, runtime plug init). Code reloading is a dev-env feature,
# so `config :phoenix, :code_reloader, true` never applies here and
# `Phoenix.CodeReloader` is compiled out of the endpoint
import_config "dev.exs"

config :logger, level: :warning

config :rockbox, RockboxWeb.Endpoint,
  debug_errors: false,
  check_origin: false

config :phoenix, :plug_init_mode, :compile
config :phoenix, :stacktrace_depth, 5
