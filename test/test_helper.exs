# Ensure the test data socket dir exists
File.mkdir_p!("/tmp/rockbox-test-data")

# Set a known engine binary path — debug build by default in test
System.put_env(
  "ROCKBOX_ENGINE_BIN",
  System.get_env("ROCKBOX_ENGINE_BIN") || "core/target/debug/engine"
)

ExUnit.start(capture_log: true)
