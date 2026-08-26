import Config

config :rockbox, RockboxWeb.Endpoint, cache_static_manifest: nil

config :logger, level: :info

# Forgeable dev tokens are a dev/test/bench convenience; prod requires
# real API keys (rb_...) issued via the admin surface.
config :rockbox, allow_dev_tokens: false
