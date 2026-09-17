defmodule Rockbox.EffectiveWireTest do
  use ExUnit.Case, async: false

  alias Rockbox.Settings.{Effective, Pipeline}

  @ctx %{workspace_id: "ws_wiretest", tier: :pro, user_id: "u1"}

  test "pipeline string capabilities survive to_wire" do
    {:ok, eff} =
      Pipeline.run(
        %{
          "language" => "python",
          "files" => [%{"path" => "main.py", "content" => "print(1)"}],
          "capabilities" => ["concurrency", "persistent_session"]
        },
        @ctx
      )

    assert eff.capabilities == ["concurrency", "persistent_session"]
    wire = Effective.to_wire(eff)
    assert wire["capabilities"] == ["concurrency", "persistent_session"]
  end

  test "mixed atom/string capabilities normalize" do
    base = %Effective{
      request_id: "req_wire",
      workspace_id: "ws",
      tier: :pro,
      language: :python,
      runtime: "python3",
      files: [],
      entrypoint: "main.py",
      mode: :exec,
      limits: %{},
      lifecycle: %{},
      capabilities: [:concurrency, "persistent_session"],
      network: %{},
      filesystem: %{},
      env: %{},
      resolved_secrets: %{},
      output: %{},
      observability: %{},
      gpu: %{},
      determinism: %{},
      cost: %{},
      labels: %{},
      session_id: nil,
      stdin: nil,
      clamped: [],
      cache_key: nil
    }

    assert Effective.to_wire(base)["capabilities"] == ["concurrency", "persistent_session"]
  end
end
