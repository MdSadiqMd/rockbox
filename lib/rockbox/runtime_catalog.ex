defmodule Rockbox.RuntimeCatalog do
  @moduledoc """
  Named Nix runtimes. Mirrors the Rust-side catalog so the engine
  and orchestrator agree on what `runtime: "python-ml"` actually means

  Adding a runtime here MUST be paired with a Nix flake under
  `nix/flakes/<name>/flake.nix` and a deployment update
  """

  alias Rockbox.RuntimeCatalog.Entry

  @entries [
    %Entry{
      name: "python-base",
      language: :python,
      baseline_caps: ["concurrency"],
      baseline_env: %{"PYTHONUNBUFFERED" => "1", "LANG" => "C.UTF-8"},
      description: "CPython 3.13, stdlib only."
    },
    %Entry{
      name: "python-ml",
      language: :python,
      baseline_caps: ["concurrency", "large_fs"],
      baseline_env: %{
        "PYTHONUNBUFFERED" => "1",
        "LANG" => "C.UTF-8",
        "MPLBACKEND" => "Agg",
        "OMP_NUM_THREADS" => "4",
        "MKL_NUM_THREADS" => "4"
      },
      description: "PyTorch + Transformers + Pandas + scikit-learn."
    },
    %Entry{
      name: "python-web",
      language: :python,
      baseline_caps: ["concurrency"],
      baseline_env: %{"PYTHONUNBUFFERED" => "1", "LANG" => "C.UTF-8"},
      description: "FastAPI + httpx + SQLAlchemy."
    },
    %Entry{
      name: "ts-modern",
      language: :typescript,
      baseline_caps: ["concurrency"],
      baseline_env: %{"NODE_NO_WARNINGS" => "1", "LANG" => "C.UTF-8"},
      description: "Node 22 + tsx + zod + axios + drizzle."
    },
    %Entry{
      name: "ts-bun",
      language: :typescript,
      baseline_caps: ["concurrency"],
      baseline_env: %{
        "BUN_INSTALL" => "/tmp/.bun",
        "XDG_CACHE_HOME" => "/tmp/.cache",
        "LANG" => "C.UTF-8"
      },
      description: "Bun 1.3 — native TypeScript, ~0.6ms boot (vs node ~13ms)."
    },
    %Entry{
      name: "go-std",
      language: :go,
      baseline_caps: ["concurrency"],
      baseline_env: %{"LANG" => "C.UTF-8"},
      description: "Go 1.23 stdlib + golang.org/x/*."
    },
    %Entry{
      name: "rust-tokio",
      language: :rust,
      baseline_caps: ["concurrency"],
      baseline_env: %{"LANG" => "C.UTF-8"},
      description: "Rust stable + tokio + serde + axum + sqlx."
    },
    %Entry{
      name: "cpp-modern",
      language: :cpp,
      baseline_caps: ["concurrency"],
      baseline_env: %{"LANG" => "C.UTF-8"},
      description: "clang 19 + libstdc++14 + abseil + boost."
    }
  ]

  @by_name Map.new(@entries, &{&1.name, &1})

  def all, do: @entries
  def lookup(name), do: Map.get(@by_name, name)

  def default_for(:python), do: @by_name["python-base"]
  def default_for(:typescript), do: @by_name["ts-modern"]
  def default_for(:go), do: @by_name["go-std"]
  def default_for(:rust), do: @by_name["rust-tokio"]
  def default_for(:cpp), do: @by_name["cpp-modern"]
  def default_for(_), do: nil
end
