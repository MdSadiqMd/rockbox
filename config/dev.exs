import Config

config :rockbox, Rockbox.Repo,
  username: System.get_env("PGUSER") || "postgres",
  password: System.get_env("PGPASSWORD") || "postgres",
  hostname: System.get_env("PGHOST") || "localhost",
  database: "rockbox_dev",
  stacktrace: true,
  show_sensitive_data_on_connection_error: true,
  pool_size: 10

config :rockbox, RockboxWeb.Endpoint,
  # Bind to all interfaces so Docker port forwarding works. On a Linux host
  # you can tighten this to {127, 0, 0, 1} if exposing 4000 publicly is a concern
  http: [ip: {0, 0, 0, 0}, port: 4000],
  check_origin: false,
  debug_errors: true,
  secret_key_base: String.duplicate("a", 64),
  watchers: []

config :logger, level: :debug
config :phoenix, :stacktrace_depth, 20
config :phoenix, :plug_init_mode, :runtime
