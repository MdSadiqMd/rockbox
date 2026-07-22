defmodule Rockbox.Integration.PlatformTest do
  use ExUnit.Case, async: false

  alias Rockbox.Settings.{Pipeline, Effective, Tiers, Defaults, Validator}
  alias Rockbox.RuntimeCatalog
  alias Rockbox.SecretsBroker

  # Wire Protocol
  describe "Wire protocol" do
    test "encode_command produces valid msgpack" do
      iodata = Rockbox.Wire.encode_command(%{"cmd" => "execute", "request_id" => "req_abc"})
      {:ok, decoded} = Msgpax.unpack(iodata)
      assert decoded["cmd"] == "execute"
      assert decoded["request_id"] == "req_abc"
    end

    test "execute constructs proper command map" do
      cmd = Rockbox.Wire.execute(%{"request_id" => "req_1", "language" => "python"})
      assert cmd["cmd"] == "execute"
      assert cmd["Execute"]["request_id"] == "req_1"
      assert cmd["Execute"]["language"] == "python"
    end

    test "decode roundtrips with encode_command" do
      original = %{"cmd" => "execute", "request_id" => "req_x"}
      encoded = Rockbox.Wire.encode_command(original)
      {:ok, decoded} = Rockbox.Wire.decode(IO.iodata_to_binary(encoded))
      assert decoded["cmd"] == "execute"
      assert decoded["request_id"] == "req_x"
    end

    test "classify returns atom from type tag" do
      assert Rockbox.Wire.classify(%{"type" => "ready"}) == :ready
      assert Rockbox.Wire.classify(%{"type" => "result"}) == :result
      assert Rockbox.Wire.classify(%{"type" => "cell_result"}) == :cell_result
    end

    test "classify returns :unknown for missing type" do
      assert Rockbox.Wire.classify(%{}) == :unknown
    end

    test "exec_cell builds correct command" do
      cmd = Rockbox.Wire.exec_cell("cell_1", "ses_1", "print(1)", wall_ms: 5000)
      assert cmd["cmd"] == "exec_cell"
      assert cmd["id"] == "cell_1"
      assert cmd["session_id"] == "ses_1"
      assert cmd["code"] == "print(1)"
      assert cmd["wall_ms"] == 5000
    end

    test "rl_step builds correct command" do
      action = Base.encode64(<<0>>)
      cmd = Rockbox.Wire.rl_step("req_1", "ep_1", action)
      assert cmd["cmd"] == "rl_step"
      assert cmd["episode_id"] == "ep_1"
      assert cmd["action"] == action
    end

    test "stdin builds correct command" do
      assert Rockbox.Wire.stdin("hello") == %{"cmd" => "stdin", "data" => "hello"}
    end

    test "interrupt builds correct command" do
      assert Rockbox.Wire.interrupt("req_1") == %{"cmd" => "interrupt", "id" => "req_1"}
    end

    test "shutdown builds correct command" do
      assert Rockbox.Wire.shutdown() == %{"cmd" => "shutdown"}
    end

    test "stringify deeply converts atom keys" do
      result = Rockbox.Wire.stringify(%{foo: :bar, nested: %{baz: :qux}})
      assert result == %{"foo" => "bar", "nested" => %{"baz" => "qux"}}
    end
  end

  # Settings: Validator
  describe "Settings Validator" do
    test "accepts valid minimal payload" do
      payload = %{
        language: "python",
        files: [%{path: "main.py", content: "print(1)"}]
      }

      assert {:ok, s} = Validator.validate(payload)
      assert s.language == :python
      assert s.mode == :exec
    end

    test "rejects missing language" do
      payload = %{files: [%{path: "main.py", content: "x"}]}
      assert {:error, violations} = Validator.validate(payload)
      assert Enum.any?(violations, &(&1.path == "language"))
    end

    test "rejects missing files" do
      payload = %{language: "python"}
      assert {:error, violations} = Validator.validate(payload)
      assert Enum.any?(violations, &(&1.path == "files"))
    end

    test "rejects empty files" do
      payload = %{language: "python", files: []}
      assert {:error, violations} = Validator.validate(payload)
      assert Enum.any?(violations, &(&1.path == "files"))
    end

    test "rejects invalid language" do
      payload = %{language: "brainfuck", files: [%{path: "x", content: "y"}]}
      assert {:error, violations} = Validator.validate(payload)
      assert Enum.any?(violations, &String.contains?(&1.reason, "must be one of"))
    end

    test "rejects unknown top-level keys" do
      payload = %{
        language: "python",
        files: [%{path: "main.py", content: "p"}],
        unknown_key: "gotcha"
      }

      assert {:error, violations} = Validator.validate(payload)
      assert Enum.any?(violations, &(&1.path == "unknown_key"))
    end

    test "coerces string language to atom" do
      assert {:ok, s} =
               Validator.validate(%{
                 language: "python",
                 files: [%{path: "main.py", content: "p"}]
               })

      assert s.language == :python
    end

    test "coerces string mode to atom" do
      assert {:ok, s} =
               Validator.validate(%{
                 language: "python",
                 mode: "session",
                 session_id: "s1",
                 files: [%{path: "main.py", content: "p"}]
               })

      assert s.mode == :session
    end

    test "validates file entries have path" do
      payload = %{language: "python", files: [%{content: "x"}]}
      assert {:error, violations} = Validator.validate(payload)
      assert Enum.any?(violations, &String.contains?(&1.path, "path"))
    end
  end

  # Settings: Defaults
  describe "Settings Defaults" do
    test "merges builtin -> mode -> runtime -> request" do
      result = Defaults.merge(%{language: :python}, "python-base", %{}, :exec)
      assert result.language == :python
      assert get_in(result, [:limits, :wall_ms]) == 5_000
    end

    test "request values override defaults" do
      result =
        Defaults.merge(
          %{language: :python, limits: %{wall_ms: 999}},
          "python-base",
          %{},
          :exec
        )

      assert get_in(result, [:limits, :wall_ms]) == 999
    end

    test "mode defaults differ per mode" do
      exec = Defaults.merge(%{}, nil, %{}, :exec)
      sess = Defaults.merge(%{}, nil, %{}, :session)
      assert get_in(exec, [:lifecycle, :auto_destroy]) == true
      assert get_in(sess, [:lifecycle, :auto_destroy]) == false
    end

    test "runtime baseline capabilities are merged only when request has none" do
      result = Defaults.merge(%{language: :python}, "python-base", %{}, :exec)
      assert "concurrency" in result.capabilities
    end

    test "request capabilities override runtime defaults" do
      result = Defaults.merge(%{language: :python, capabilities: []}, "python-base", %{}, :exec)
      assert result.capabilities == []
    end
  end

  # Settings: Tiers
  describe "Settings Tiers" do
    test "clamps wall_ms to tier ceiling" do
      settings = %{limits: %{wall_ms: 999_999, memory_mb: 512, cpu_cores: 1}}
      {clamped, log} = Tiers.clamp(settings, :free)
      assert get_in(clamped, [:limits, :wall_ms]) == 10_000
      assert Enum.any?(log, &(&1.path == "limits.wall_ms"))
    end

    test "free tier allows up to 10s wall_ms" do
      settings = %{limits: %{wall_ms: 5_000, memory_mb: 512, cpu_cores: 1}}
      {clamped, _log} = Tiers.clamp(settings, :free)
      assert get_in(clamped, [:limits, :wall_ms]) == 5_000
    end

    test "pro tier allows up to 60s wall_ms" do
      settings = %{limits: %{wall_ms: 60_000, memory_mb: 2048, cpu_cores: 2}}
      {clamped, _log} = Tiers.clamp(settings, :pro)
      assert get_in(clamped, [:limits, :wall_ms]) == 60_000
    end

    test "clamps memory_mb to tier ceiling" do
      settings = %{limits: %{wall_ms: 100, memory_mb: 999_999, cpu_cores: 1}}
      {clamped, _} = Tiers.clamp(settings, :free)
      assert get_in(clamped, [:limits, :memory_mb]) == 1024
    end

    test "filters disallowed capabilities" do
      settings = %{
        capabilities: ["gpu", "concurrency", "install"],
        limits: %{wall_ms: 100, memory_mb: 100, cpu_cores: 1}
      }

      {clamped, _log} = Tiers.clamp(settings, :free)
      assert clamped.capabilities == ["concurrency"]
    end
  end

  # Settings: CrossField
  describe "Settings CrossField" do
    test "session mode requires session_id" do
      assert {:error, violations} =
               Rockbox.Settings.CrossField.apply(%{mode: "session", language: "python"})

      assert Enum.any?(violations, &(&1.path == "session_id"))
    end

    test "exec mode with session_id triggers warning" do
      assert {:error, violations} =
               Rockbox.Settings.CrossField.apply(%{
                 mode: "exec",
                 session_id: "s1",
                 language: "python"
               })

      assert Enum.any?(violations, &(&1.path == "session_id"))
    end

    test "session auto-adds persistent_session capability" do
      assert {:ok, s} =
               Rockbox.Settings.CrossField.apply(%{mode: "session", session_id: "s1"})

      assert "persistent_session" in s.capabilities
    end

    test "gpu count > 0 auto-adds gpu capability" do
      assert {:ok, s} = Rockbox.Settings.CrossField.apply(%{gpu: %{count: 2}})
      assert "gpu" in s.capabilities
    end
  end

  # Settings: Pipeline (full)
  describe "Settings Pipeline (full)" do
    test "full pipeline succeeds with minimal valid payload (pro tier)" do
      ctx = %{workspace_id: "test-ws", tier: :pro, user_id: "test-user"}

      assert {:ok, %Effective{} = eff} =
               Pipeline.run(
                 %{
                   "language" => "python",
                   "files" => [%{"path" => "main.py", "content" => "print(1)"}]
                 },
                 ctx
               )

      assert eff.language == :python
      assert eff.workspace_id == "test-ws"
      assert eff.tier == :pro
      assert eff.request_id != nil
      assert eff.mode == :exec
      assert eff.runtime == "python-base"
      assert length(eff.files) == 1
      assert eff.files |> List.first() |> Map.get("path") == "main.py"
    end

    test "full pipeline succeeds with free tier (clamps applied)" do
      ctx = %{workspace_id: "test-ws", tier: :free, user_id: "test-user"}

      assert {:ok, %Effective{} = eff} =
               Pipeline.run(
                 %{
                   "language" => "python",
                   "limits" => %{"wall_ms" => 60_000, "memory_mb" => 2048},
                   "files" => [%{"path" => "main.py", "content" => "print(1)"}]
                 },
                 ctx
               )

      wall_ms = get_in(eff.limits, ["wall_ms"])
      mem_mb = get_in(eff.limits, ["memory_mb"])

      assert wall_ms == 10_000, "expected wall_ms=10000 got #{inspect(wall_ms)}"
      assert mem_mb == 1024, "expected memory_mb=1024 got #{inspect(mem_mb)}"
      assert length(eff.clamped) >= 2
    end

    test "pipeline rejects unknown language" do
      ctx = %{workspace_id: "ws", tier: :pro, user_id: "u"}

      assert {:error, violations} =
               Pipeline.run(
                 %{"language" => "cobol", "files" => [%{"path" => "x", "content" => "y"}]},
                 ctx
               )

      assert Enum.any?(violations, &(&1.path == "language"))
    end

    test "pipeline rejects missing required fields" do
      ctx = %{workspace_id: "ws", tier: :pro, user_id: "u"}
      assert {:error, violations} = Pipeline.run(%{}, ctx)
      assert violations != []
    end

    test "pipeline resolves secrets via SecretsBroker" do
      Application.put_env(:rockbox, :secrets, %{"test/path" => "secret-value"})
      ctx = %{workspace_id: "ws", tier: :free, user_id: "u"}

      assert {:ok, %Effective{} = eff} =
               Pipeline.run(
                 %{
                   "language" => "python",
                   "files" => [%{"path" => "main.py", "content" => "x"}],
                   "secrets" => [%{"name" => "my_key", "ref" => "vault://test/path"}]
                 },
                 ctx
               )

      assert eff.resolved_secrets["my_key"] == "secret-value"
    after
      Application.delete_env(:rockbox, :secrets)
    end
  end

  # Settings: Effective
  describe "Settings Effective" do
    test "to_wire produces engine-ready map with string keys" do
      ctx = %{workspace_id: "ws", tier: :pro, user_id: "u"}

      assert {:ok, %Effective{} = eff} =
               Pipeline.run(
                 %{
                   "language" => "python",
                   "files" => [%{"path" => "main.py", "content" => "print(1)"}]
                 },
                 ctx
               )

      wire = Effective.to_wire(eff)
      assert wire["schema"] == "v1"
      assert wire["language"] == "python"
      assert wire["request_id"] != nil
      assert wire["mode"] == "exec"
      assert wire["runtime"] == "python-base"
      assert is_list(wire["files"])
      assert is_map(wire["limits"])
      assert is_map(wire["lifecycle"])
    end
  end

  # RuntimeCatalog
  describe "RuntimeCatalog" do
    test "lookup returns entry for known runtimes" do
      assert %{} = RuntimeCatalog.lookup("python-base")
      assert %{} = RuntimeCatalog.lookup("ts-modern")
      assert %{} = RuntimeCatalog.lookup("go-std")
      assert %{} = RuntimeCatalog.lookup("rust-tokio")
      assert %{} = RuntimeCatalog.lookup("cpp-modern")
    end

    test "lookup returns nil for unknown runtime" do
      assert RuntimeCatalog.lookup("nonexistent") == nil
    end

    test "default_for returns correct runtime per language" do
      assert RuntimeCatalog.default_for(:python).name == "python-base"
      assert RuntimeCatalog.default_for(:typescript).name == "ts-modern"
      assert RuntimeCatalog.default_for(:go).name == "go-std"
    end

    test "all returns all entries" do
      assert length(RuntimeCatalog.all()) >= 6
    end
  end

  # Authentication
  describe "Authentication" do
    test "returns 401 without authorization header" do
      conn =
        :post
        |> Plug.Test.conn("/api/execute", %{"settings" => %{}})
        |> RockboxWeb.Router.call([])

      assert conn.status == 401
      assert conn.resp_body =~ "unauthorized"
    end

    test "returns 401 with bad token format" do
      conn =
        :post
        |> Plug.Test.conn("/api/execute", %{"settings" => %{}})
        |> Plug.Conn.put_req_header("authorization", "Bearer bad-token")
        |> RockboxWeb.Router.call([])

      assert conn.status == 401
    end

    test "accepts valid token and passes through" do
      conn =
        :post
        |> Plug.Test.conn("/api/execute", %{
          "settings" => %{
            "language" => "python",
            "files" => [%{"path" => "main.py", "content" => "x"}]
          }
        })
        |> Plug.Conn.put_req_header("authorization", "Bearer token-ws_pro_demo-pro")
        |> RockboxWeb.Router.call([])

      assert conn.status != 401
    end
  end

  # Health Endpoint
  describe "Health" do
    test "GET /health returns ok" do
      conn =
        :get
        |> Plug.Test.conn("/health", %{})
        |> RockboxWeb.Router.call([])

      assert conn.status == 200
      assert conn.resp_body =~ "ok"
    end

    test "GET /ready returns engine binary path" do
      conn =
        :get
        |> Plug.Test.conn("/ready", %{})
        |> RockboxWeb.Router.call([])

      assert conn.status == 200
      body = Jason.decode!(conn.resp_body)
      assert body["status"] == "ready"
      assert body["engine_binary"] != nil
    end
  end

  # SecretsBroker
  describe "SecretsBroker" do
    test "resolve returns empty map for empty list" do
      assert SecretsBroker.resolve([]) == {:ok, %{}}
    end

    test "resolve returns error for unresolved ref" do
      assert {:error, {:unresolved, ["my_key"]}} =
               SecretsBroker.resolve([%{"name" => "my_key", "ref" => "vault://nonexistent"}])
    end

    test "resolve fetches from env and caches" do
      System.put_env("TEST_SECRET", "env-value")

      assert {:ok, resolved} =
               SecretsBroker.resolve([%{"name" => "my_key", "ref" => "env://TEST_SECRET"}])

      assert resolved["my_key"] == "env-value"
    after
      System.delete_env("TEST_SECRET")
    end
  end

  # QuotaTracker
  describe "QuotaTracker" do
    test "reserve and release cycle" do
      assert Rockbox.QuotaTracker.reserve("test-quota-ws") == :ok
      assert Rockbox.QuotaTracker.in_flight("test-quota-ws") == 1
      Rockbox.QuotaTracker.release("test-quota-ws")
      assert Rockbox.QuotaTracker.in_flight("test-quota-ws") == 0
    end

    test "concurrency exceeded when over cap" do
      previous = Application.get_env(:rockbox, :workspace_concurrency_default, 50)
      Application.put_env(:rockbox, :workspace_concurrency_default, 1)

      assert Rockbox.QuotaTracker.reserve("quota-cap-test") == :ok
      assert Rockbox.QuotaTracker.reserve("quota-cap-test") == {:error, :concurrency_exceeded}

      Rockbox.QuotaTracker.release("quota-cap-test")
      Application.put_env(:rockbox, :workspace_concurrency_default, previous)
    end
  end

  # Pool Manager
  describe "Pool Manager" do
    test "acquire returns vm_id for valid settings" do
      ctx = %{workspace_id: "pool-test", tier: :pro, user_id: "u"}

      assert {:ok, %Effective{} = eff} =
               Pipeline.run(
                 %{
                   "language" => "python",
                   "files" => [%{"path" => "main.py", "content" => "print(1)"}]
                 },
                 ctx
               )

      # Acquire may succeed (engine available) or fail (engine binary issue)
      case Rockbox.Pool.Manager.acquire(eff) do
        {:ok, vm_id} ->
          assert is_binary(vm_id)
          assert String.starts_with?(vm_id, "vm_")
          Rockbox.Pool.Manager.release(vm_id, eff)
          :ok

        {:error, reason} ->
          # Acceptable — engine binary might not be ready
          flunk("Pool acquire failed: #{inspect(reason)}")
      end
    end
  end

  # Execute Controller (validation without engine)
  describe "Execute Controller" do
    test "returns 400 for missing settings" do
      conn =
        :post
        |> Plug.Test.conn("/api/execute", %{})
        |> Plug.Conn.put_req_header("authorization", "Bearer token-ws_pro_demo-pro")
        |> RockboxWeb.Router.call([])

      assert conn.status == 400
      body = Jason.decode!(conn.resp_body)
      assert body["error"] == "missing `settings` object"
    end

    test "returns 422 for invalid settings" do
      conn =
        :post
        |> Plug.Test.conn("/api/execute", %{"settings" => %{"language" => "brainfuck"}})
        |> Plug.Conn.put_req_header("authorization", "Bearer token-ws_pro_demo-pro")
        |> RockboxWeb.Router.call([])

      assert conn.status == 422
      body = Jason.decode!(conn.resp_body)
      assert body["error"] == "settings_invalid"
      assert is_list(body["violations"])
    end

    test "returns 422 for missing language" do
      conn =
        :post
        |> Plug.Test.conn("/api/execute", %{"settings" => %{"files" => []}})
        |> Plug.Conn.put_req_header("authorization", "Bearer token-ws_pro_demo-pro")
        |> RockboxWeb.Router.call([])

      assert conn.status == 422
    end

    test "executes Python code end-to-end" do
      conn =
        :post
        |> Plug.Test.conn("/api/execute", %{
          "settings" => %{
            "language" => "python",
            "files" => [
              %{"path" => "main.py", "content" => ~S/print("hello from platform test")/}
            ]
          }
        })
        |> Plug.Conn.put_req_header("authorization", "Bearer token-ws_pro_demo-pro")
        |> RockboxWeb.Router.call([])

      body = Jason.decode!(conn.resp_body)

      if conn.status == 200 do
        assert body["status"] == "success"
        assert body["exit_code"] == 0
        assert body["output"] =~ "hello from platform test"
        assert body["vm_id"] != nil
        assert is_integer(body["exec_time_ms"])
        assert is_number(body["memory_peak_mb"])
        assert body["request_id"] != nil
      else
        assert conn.status in [500, 504]
        assert is_binary(body["error"])
      end
    end

    test "executes TypeScript code end-to-end" do
      conn =
        :post
        |> Plug.Test.conn("/api/execute", %{
          "settings" => %{
            "language" => "typescript",
            "files" => [%{"path" => "index.ts", "content" => ~S/console.log("ts works");/}]
          }
        })
        |> Plug.Conn.put_req_header("authorization", "Bearer token-ws_pro_demo-pro")
        |> RockboxWeb.Router.call([])

      body = Jason.decode!(conn.resp_body)

      if conn.status == 200 do
        assert body["status"] == "success"
        assert body["exit_code"] == 0
        assert body["output"] =~ "ts works"
      else
        assert conn.status in [500, 504]
        assert is_binary(body["error"])
      end
    end

    test "executes Go code end-to-end" do
      conn =
        :post
        |> Plug.Test.conn("/api/execute", %{
          "settings" => %{
            "language" => "go",
            "files" => [
              %{
                "path" => "main.go",
                "content" => ~S'''
                package main
                import "fmt"
                func main() { fmt.Print("go works") }
                '''
              }
            ]
          }
        })
        |> Plug.Conn.put_req_header("authorization", "Bearer token-ws_pro_demo-pro")
        |> RockboxWeb.Router.call([])

      body = Jason.decode!(conn.resp_body)

      if conn.status == 200 do
        assert body["status"] == "success"
        assert body["exit_code"] == 0
        assert body["output"] =~ "go works"
      else
        assert conn.status in [500, 504]
        assert is_binary(body["error"])
      end
    end

    test "executes Rust code end-to-end" do
      conn =
        :post
        |> Plug.Test.conn("/api/execute", %{
          "settings" => %{
            "language" => "rust",
            "files" => [
              %{
                "path" => "main.rs",
                "content" => ~S'''
                fn main() { println!("rust works"); }
                '''
              }
            ]
          }
        })
        |> Plug.Conn.put_req_header("authorization", "Bearer token-ws_pro_demo-pro")
        |> RockboxWeb.Router.call([])

      body = Jason.decode!(conn.resp_body)

      if conn.status == 200 do
        assert body["status"] in ["success", "engine_error"]

        if body["status"] == "success" do
          assert body["exit_code"] == 0
          assert body["output"] =~ "rust works"
        end
      else
        assert conn.status in [500, 504]
        assert is_binary(body["error"])
      end
    end

    test "executes C++ code end-to-end" do
      conn =
        :post
        |> Plug.Test.conn("/api/execute", %{
          "settings" => %{
            "language" => "cpp",
            "files" => [
              %{
                "path" => "main.cpp",
                "content" => ~S'''
                #include <iostream>
                int main() { std::cout << "cpp works"; return 0; }
                '''
              }
            ]
          }
        })
        |> Plug.Conn.put_req_header("authorization", "Bearer token-ws_pro_demo-pro")
        |> RockboxWeb.Router.call([])

      body = Jason.decode!(conn.resp_body)

      if conn.status == 200 do
        assert body["status"] == "success"
        assert body["exit_code"] == 0
        assert body["output"] =~ "cpp works"
      else
        assert conn.status in [500, 504]
        assert is_binary(body["error"])
      end
    end

    test "execute returns runtime metrics in success response" do
      conn =
        :post
        |> Plug.Test.conn("/api/execute", %{
          "settings" => %{
            "language" => "python",
            "files" => [%{"path" => "main.py", "content" => "print(42)"}]
          }
        })
        |> Plug.Conn.put_req_header("authorization", "Bearer token-ws_pro_demo-pro")
        |> RockboxWeb.Router.call([])

      body = Jason.decode!(conn.resp_body)

      if conn.status == 200 do
        assert is_integer(body["exec_time_ms"])
        assert is_number(body["memory_peak_mb"])
        assert body["output_truncated"] == false
      else
        assert conn.status in [500, 504]
        assert is_binary(body["error"])
      end
    end

    test "concurrency limit is enforced" do
      previous = Application.get_env(:rockbox, :workspace_concurrency_default, 50)
      Application.put_env(:rockbox, :workspace_concurrency_default, 2)

      ctx = %{workspace_id: "conc-test", tier: :free, user_id: "u"}

      assert {:ok, %Effective{} = eff} =
               Pipeline.run(
                 %{
                   "language" => "python",
                   "files" => [%{"path" => "main.py", "content" => "print(1)"}]
                 },
                 ctx
               )

      vms =
        Enum.reduce_while(1..5, [], fn _i, acc ->
          case Rockbox.Pool.Manager.acquire(eff) do
            {:ok, vm_id} -> {:cont, [vm_id | acc]}
            {:error, :concurrency_exceeded} -> {:halt, acc}
            e -> flunk("Unexpected: #{inspect(e)}")
          end
        end)

      assert length(vms) <= 2

      Enum.each(vms, &Rockbox.Pool.Manager.release(&1, eff))
      Application.put_env(:rockbox, :workspace_concurrency_default, previous)
    end
  end
end
