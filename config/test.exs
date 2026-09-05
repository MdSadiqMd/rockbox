import Config

config :rockbox, Rockbox.Repo,
  username: System.get_env("PGUSER") || "postgres",
  password: System.get_env("PGPASSWORD") || "postgres",
  hostname: System.get_env("PGHOST") || "localhost",
  database: "rockbox_test#{System.get_env("MIX_TEST_PARTITION")}",
  pool: Ecto.Adapters.SQL.Sandbox,
  pool_size: System.schedulers_online() * 2

config :rockbox, :engine,
  binary: System.get_env("ROCKBOX_ENGINE_BIN") || "core/target/debug/engine",
  data_socket_dir: System.get_env("ROCKBOX_DATA_SOCK_DIR") || "/tmp/rockbox-test-data"

config :rockbox, RockboxWeb.Endpoint,
  http: [ip: {127, 0, 0, 1}, port: 4002],
  secret_key_base: String.duplicate("a", 64),
  server: false

config :logger, level: :warning
config :phoenix, :plug_init_mode, :runtime

# Rate limiting is exercised by dedicated plug tests only; keep the bucket
# effectively unlimited elsewhere so suite-wide request volume never 429s.
config :rockbox, rate_burst: 1_000_000, rate_per_s: 1_000_000

# Exec cache is a warm-path optimization for repeated programs. In test each
# `POST /api/execute` is expected to hit the engine exactly once so
# determinism and quota tests don't see cross-test hits. Disabled here;
# bench/prod keep it enabled (SOTA loop 2).
config :rockbox, exec_cache_enabled: false
