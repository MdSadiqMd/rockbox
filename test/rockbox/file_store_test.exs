defmodule Rockbox.FileStoreTest do
  use ExUnit.Case, async: true

  alias Rockbox.FileStore

  setup do
    root =
      Path.join(
        System.tmp_dir!(),
        "rockbox_filestore_test_#{:erlang.unique_integer([:positive])}"
      )

    File.mkdir_p!(Path.join(root, "sub"))
    File.write!(Path.join(root, "hello.txt"), "hi")
    File.write!(Path.join(root, "secret.txt"), "top")

    on_exit(fn -> File.rm_rf!(root) end)
    {:ok, root: root}
  end

  describe "safe_path/2" do
    test "accepts plain and nested paths", %{root: root} do
      assert {:ok, path} = FileStore.safe_path(root, "a.txt")
      assert path == Path.join(root, "a.txt")

      assert {:ok, path} = FileStore.safe_path(root, "/sub/a.txt")
      assert path == Path.join(root, "sub/a.txt")
      assert {:ok, ^root} = FileStore.safe_path(root, "/")
      assert {:ok, ^root} = FileStore.safe_path(root, "")
    end

    test "rejects traversal", %{root: root} do
      assert {:error, reason} = FileStore.safe_path(root, "../etc/passwd")
      assert reason in [:invalid_path, :outside_root]

      assert {:error, reason} = FileStore.safe_path(root, "sub/../../x")
      assert reason in [:invalid_path, :outside_root]

      assert {:error, reason} =
               FileStore.safe_path(root, "/#{String.trim_leading(root, "/")}/../secret.txt")

      assert reason in [:invalid_path, :outside_root]
    end
  end

  describe "read/2" do
    test "reads a file inside the root", %{root: root} do
      assert {:ok, %{path: "/hello.txt", size: 2, truncated: false, content_b64: b64}} =
               FileStore.read(root, "hello.txt")

      assert Base.decode64!(b64) == "hi"
    end

    test "missing file", %{root: root} do
      assert {:error, :enoent} = FileStore.read(root, "nope.txt")
    end

    test "symlink escaping the root is refused", %{root: root} do
      outside = Path.join(Path.dirname(root), "outside_#{:erlang.unique_integer([:positive])}")
      File.write!(outside, "stolen")
      on_exit(fn -> File.rm(outside) end)

      File.mkdir_p!(Path.join(root, "sub"))
      File.ln_s!(outside, Path.join([root, "sub", "leak"]))

      assert {:error, :outside_root} = FileStore.read(root, "sub/leak")
    end

    test "deep symlink chain out of the root is refused", %{root: root} do
      outside_dir = Path.join(Path.dirname(root), "outd_#{:erlang.unique_integer([:positive])}")
      File.mkdir_p!(outside_dir)
      target = Path.join(outside_dir, "prize")
      File.write!(target, "nope")
      on_exit(fn -> File.rm_rf!(outside_dir) end)

      link1 = Path.join(root, "l1")
      link2 = Path.join(root, "l2")
      File.ln_s!(target, link1)
      File.ln_s!(link1, link2)

      assert {:error, :outside_root} = FileStore.read(root, "l2")
    end

    test "large files are truncated at the cap", %{root: root} do
      big = String.duplicate("x", FileStore.max_read_bytes() + 1000)
      File.write!(Path.join(root, "big.bin"), big)

      assert {:ok, %{truncated: true, content_b64: b64}} = FileStore.read(root, "big.bin")
      assert byte_size(Base.decode64!(b64)) == FileStore.max_read_bytes()
    end
  end

  describe "list/2" do
    test "lists entries with metadata", %{root: root} do
      assert {:ok, entries} = FileStore.list(root, "/")
      names = Enum.map(entries, & &1.name)
      assert "hello.txt" in names and "sub" in names

      hello = Enum.find(entries, &(&1.name == "hello.txt"))
      assert hello.type == "regular" and hello.size == 2 and hello.path == "/hello.txt"
    end

    test "listing a file path errors", %{root: root} do
      assert {:error, _} = FileStore.list(root, "hello.txt")
    end
  end

  describe "write/3 + remove/2" do
    test "round-trips a nested write", %{root: root} do
      payload = Base.encode64("data123")

      assert {:ok, %{path: "/new/dir/f.bin", size: 7}} =
               FileStore.write(root, "/new/dir/f.bin", payload)

      assert {:ok, %{content_b64: b64}} = FileStore.read(root, "/new/dir/f.bin")
      assert Base.decode64!(b64) == "data123"

      assert {:ok, %{deleted: "/new"}} = FileStore.remove(root, "/new")
      refute File.exists?(Path.join(root, "new"))
    end

    test "rejects invalid base64", %{root: root} do
      assert {:error, :invalid_base64} = FileStore.write(root, "f.txt", "!!!not-base64!!!")
    end

    test "refuses to remove the volume root itself", %{root: root} do
      assert {:error, :cannot_remove_root} = FileStore.remove(root, "/")
    end

    test "refuses writes that resolve outside the root via symlink", %{root: root} do
      outside_dir = Path.join(Path.dirname(root), "outw_#{:erlang.unique_integer([:positive])}")
      File.mkdir_p!(outside_dir)
      on_exit(fn -> File.rm_rf!(outside_dir) end)
      File.ln_s!(outside_dir, Path.join(root, "escape"))

      assert {:error, :outside_root} =
               FileStore.write(root, "escape/evil.txt", Base.encode64("x"))

      refute File.exists?(Path.join(outside_dir, "evil.txt"))
    end
  end
end
