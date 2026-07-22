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
