import Config

if System.get_env("PHX_SERVER") do
  config :rockbox, RockboxWeb.Endpoint, server: true
end

maybe_ipv6 = fn
  nil -> []
  _ -> [:inet6]
end

if config_env() == :prod do
  database_url =
    System.get_env("DATABASE_URL") ||
      raise """
      environment variable DATABASE_URL is missing.
      Example: ecto://USER:PASS@HOST/DATABASE
      """

  config :rockbox, Rockbox.Repo,
    url: database_url,
    pool_size: String.to_integer(System.get_env("POOL_SIZE", "10")),
    socket_options: maybe_ipv6.(System.get_env("ECTO_IPV6"))

  secret_key_base =
    System.get_env("SECRET_KEY_BASE") ||
      raise "environment variable SECRET_KEY_BASE is missing."

  host = System.get_env("PHX_HOST") || "rockbox.local"
  port = String.to_integer(System.get_env("PORT") || "4000")

  config :rockbox, RockboxWeb.Endpoint,
    url: [host: host, port: 443, scheme: "https"],
    http: [ip: {0, 0, 0, 0, 0, 0, 0, 0}, port: port],
    secret_key_base: secret_key_base

  config :rockbox, :engine,
    binary: System.get_env("ROCKBOX_ENGINE_BIN") || "/opt/rockbox/bin/engine",
    data_socket_dir: System.get_env("ROCKBOX_DATA_SOCK_DIR") || "/run/rockbox/data"
end

config :rockbox,
       :workspace_concurrency_default,
       String.to_integer(System.get_env("WORKSPACE_CONCURRENCY", "50"))

config :rockbox,
  rate_burst: String.to_integer(System.get_env("RATE_BURST", "100")),
  rate_per_s: String.to_integer(System.get_env("RATE_PER_S", "50"))

# API keys + admin surface. Dev tokens are a dev/test/bench convenience; in
# prod they are off unless explicitly re-enabled. The admin token gates
# /api/admin/* (workspace + key provisioning) — unset means disabled.
parse_flag = fn
  "" -> nil
  v -> String.equivalent?(v, "true")
end

config :rockbox,
  allow_dev_tokens: parse_flag.(System.get_env("ROCKBOX_ALLOW_DEV_TOKENS", "")),
  admin_token: System.get_env("ROCKBOX_ADMIN_TOKEN", "")

# User-defined environments: where generated flakes live and where engine
# descriptors are dropped (must match the engine's ROCKBOX_CUSTOM_RUNTIMES_DIR).
config :rockbox,
  environments_root: System.get_env("ROCKBOX_ENVIRONMENTS_ROOT", "/var/lib/rockbox/envs"),
  custom_runtimes_dir:
    System.get_env("ROCKBOX_CUSTOM_RUNTIMES_DIR", "/etc/sandbox/custom-runtimes")
