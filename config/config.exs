import Config

config :rockbox,
  ecto_repos: [Rockbox.Repo],
  generators: [timestamp_type: :utc_datetime_usec, binary_id: true]

config :rockbox, Rockbox.Repo,
  migration_primary_key: [name: :id, type: :binary_id],
  migration_foreign_key: [column: :id, type: :binary_id]

# Engine binary the VMs spawn (built by `make engine` / `cargo build`)
config :rockbox, :engine,
  binary: System.get_env("ROCKBOX_ENGINE_BIN") || "core/target/release/engine",
  data_socket_dir: System.get_env("ROCKBOX_DATA_SOCK_DIR") || "/tmp/rockbox-data"

# Loop27: the RL stepping endpoints negotiate raw-bytes ticks via
# `Accept: application/msgpack` (SDK default). Register the type so the
# `:accepts` plug lets it through instead of 406ing before the action runs.
config :mime, :types, %{"application/msgpack" => ["msgpack"]}

config :rockbox,
  episodes_root: System.get_env("ROCKBOX_EPISODES_ROOT") || "/var/lib/sandbox/episodes",
  episode_routes_path:
    System.get_env("ROCKBOX_EPISODE_ROUTES_PATH") || "/var/lib/rockbox/episode-routes.dets"

# Defaults for the Settings pipeline. Workspaces + tiers override these
config :rockbox, :settings_defaults, %{
  limits: %{
    wall_ms: 5_000,
    compile_ms: 30_000,
    memory_mb: 512,
    cpu_cores: 1.0,
    pids_max: 200,
    fd_max: 256,
    fsize_mb: 50,
    tmpfs_mb: 512,
    stack_mb: 8,
    output_bytes: 2 * 1024 * 1024,
    output_action: "truncate"
  },
  lifecycle: %{
    idle_ttl_s: 1800,
    max_lifetime_s: 86_400,
    auto_destroy: true,
    keep_alive_on_error: false,
    restart_policy: "none"
  },
  network: %{
    tier: "none",
    block_metadata: true,
    dns: %{mode: "proxied", cache_s: 60}
  },
  output: %{
    stream: true,
    binary_safe: false,
    merge_streams: false,
    include_metrics: true
  },
  observability: %{capture_metrics: true, trace_syscalls: false}
}

config :rockbox, :tiers,
  free: %{
    wall_ms_max: 10_000,
    memory_mb_max: 1024,
    cpu_cores_max: 1.0,
    network_tier_max: "none",
    gpu_count_max: 0,
    concurrent_max: 5,
    capabilities_allowed: ~w(concurrency)
  },
  pro: %{
    wall_ms_max: 60_000,
    memory_mb_max: 4096,
    cpu_cores_max: 4.0,
    network_tier_max: "egress-allowlist",
    gpu_count_max: 1,
    concurrent_max: 50,
    capabilities_allowed: ~w(concurrency subprocess large_fs persistent_session)
  },
  enterprise: %{
    wall_ms_max: 300_000,
    memory_mb_max: 32_768,
    cpu_cores_max: 16.0,
    network_tier_max: "egress-open",
    gpu_count_max: 8,
    concurrent_max: 100_000,
    capabilities_allowed:
      ~w(concurrency subprocess large_fs persistent_session install raw_sockets gpu)
  }

config :phoenix, :json_library, Jason

config :rockbox, RockboxWeb.Endpoint,
  url: [host: "localhost"],
  adapter: Bandit.PhoenixAdapter,
  render_errors: [
    formats: [json: RockboxWeb.ErrorJSON],
    layout: false
  ],
  pubsub_server: Rockbox.PubSub,
  live_view: [signing_salt: "rockbox-salt"]

config :logger, :default_formatter,
  format: "$time $metadata[$level] $message\n",
  metadata: [:request_id, :workspace, :user, :vm_id, :session_id]

import_config "#{config_env()}.exs"
