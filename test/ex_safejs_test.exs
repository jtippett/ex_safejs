defmodule ExSafejsTest do
  use ExUnit.Case
  use ExUnitProperties

  describe "start/stop" do
    test "start and stop runtime" do
      {:ok, rt} = ExSafejs.start()
      assert :ok = ExSafejs.stop(rt)
    end

    test "start with custom options" do
      {:ok, rt} = ExSafejs.start(memory_limit: 10_000_000, timeout: 5_000)
      assert :ok = ExSafejs.stop(rt)
    end

    test "stop is idempotent" do
      {:ok, rt} = ExSafejs.start()
      assert :ok = ExSafejs.stop(rt)
      assert :ok = ExSafejs.stop(rt)
    end

    test "eval on stopped runtime returns error" do
      {:ok, rt} = ExSafejs.start()
      ExSafejs.stop(rt)
      assert {:error, "dead_runtime"} = ExSafejs.eval(rt, "1")
    end

    test "eval with callbacks on stopped runtime returns error" do
      {:ok, rt} = ExSafejs.start()
      ExSafejs.stop(rt)
      callbacks = %{"f" => fn [] -> {:ok, nil} end}
      assert {:error, "dead_runtime"} = ExSafejs.eval(rt, "f()", callbacks)
    end

    test "alive? returns true for running runtime" do
      {:ok, rt} = ExSafejs.start()
      assert ExSafejs.alive?(rt)
      ExSafejs.stop(rt)
    end

    test "alive? returns false after stop" do
      {:ok, rt} = ExSafejs.start()
      ExSafejs.stop(rt)
      refute ExSafejs.alive?(rt)
    end
  end

  describe "eval/2" do
    setup do
      {:ok, rt} = ExSafejs.start()
      on_exit(fn -> ExSafejs.stop(rt) end)
      %{rt: rt}
    end

    test "arithmetic", %{rt: rt} do
      assert {:ok, 3} = ExSafejs.eval(rt, "1 + 2")
      assert {:ok, 42} = ExSafejs.eval(rt, "21 * 2")
    end

    test "strings", %{rt: rt} do
      assert {:ok, "hello"} = ExSafejs.eval(rt, "'hello'")
      assert {:ok, "hello world"} = ExSafejs.eval(rt, "'hello' + ' ' + 'world'")
    end

    test "booleans", %{rt: rt} do
      assert {:ok, true} = ExSafejs.eval(rt, "true")
      assert {:ok, false} = ExSafejs.eval(rt, "false")
    end

    test "null and undefined", %{rt: rt} do
      assert {:ok, nil} = ExSafejs.eval(rt, "null")
      assert {:ok, nil} = ExSafejs.eval(rt, "undefined")
    end

    test "arrays", %{rt: rt} do
      assert {:ok, [1, 2, 3]} = ExSafejs.eval(rt, "[1, 2, 3]")
    end

    test "objects", %{rt: rt} do
      assert {:ok, %{"a" => 1, "b" => 2}} = ExSafejs.eval(rt, "({a: 1, b: 2})")
    end

    test "floats", %{rt: rt} do
      assert {:ok, 3.14} = ExSafejs.eval(rt, "3.14")
    end

    test "large integers", %{rt: rt} do
      assert {:ok, 9_007_199_254_740_991} = ExSafejs.eval(rt, "Number.MAX_SAFE_INTEGER")
      assert {:ok, -9_007_199_254_740_991} = ExSafejs.eval(rt, "-Number.MAX_SAFE_INTEGER")
    end

    test "NaN and Infinity become nil", %{rt: rt} do
      assert {:ok, nil} = ExSafejs.eval(rt, "NaN")
      assert {:ok, nil} = ExSafejs.eval(rt, "Infinity")
      assert {:ok, nil} = ExSafejs.eval(rt, "-Infinity")
    end

    test "functions in objects are stripped", %{rt: rt} do
      assert {:ok, %{"x" => 1}} = ExSafejs.eval(rt, "({x: 1, fn: function() {}})")
    end

    test "empty array and object", %{rt: rt} do
      assert {:ok, []} = ExSafejs.eval(rt, "[]")
      assert {:ok, %{}} = ExSafejs.eval(rt, "({})")
    end

    test "nested structures", %{rt: rt} do
      assert {:ok, %{"items" => [1, 2], "name" => "test"}} =
               ExSafejs.eval(rt, "({name: 'test', items: [1, 2]})")
    end

    test "deeply nested structure", %{rt: rt} do
      # Build a 60-level deep nested object (under MAX_DEPTH of 64)
      code = "var o = {v: 1}; for (var i = 0; i < 59; i++) { o = {n: o}; } o"
      assert {:ok, result} = ExSafejs.eval(rt, code)
      assert is_map(result)
    end

    test "syntax error", %{rt: rt} do
      assert {:error, msg} = ExSafejs.eval(rt, "function {")
      assert is_binary(msg)
    end

    test "runtime error", %{rt: rt} do
      assert {:error, msg} = ExSafejs.eval(rt, "undefinedVar.prop")
      assert is_binary(msg)
    end

    test "thrown error", %{rt: rt} do
      assert {:error, msg} = ExSafejs.eval(rt, "throw new Error('boom')")
      assert msg =~ "boom"
    end

    test "global state persists", %{rt: rt} do
      assert {:ok, 42} = ExSafejs.eval(rt, "globalThis.x = 42")
      assert {:ok, 42} = ExSafejs.eval(rt, "x")
    end
  end

  describe "eval/3 callbacks" do
    setup do
      {:ok, rt} = ExSafejs.start()
      on_exit(fn -> ExSafejs.stop(rt) end)
      %{rt: rt}
    end

    test "single callback", %{rt: rt} do
      callbacks = %{
        "greet" => fn [name] -> {:ok, "Hello, #{name}!"} end
      }

      assert {:ok, "Hello, Alice!"} = ExSafejs.eval(rt, "greet('Alice')", callbacks)
    end

    test "callback with multiple args", %{rt: rt} do
      callbacks = %{
        "add" => fn [a, b] -> {:ok, a + b} end
      }

      assert {:ok, 5} = ExSafejs.eval(rt, "add(2, 3)", callbacks)
    end

    test "callback returning object", %{rt: rt} do
      callbacks = %{
        "get_user" => fn [id] -> {:ok, %{"id" => id, "name" => "User #{id}"}} end
      }

      assert {:ok, "User 1"} = ExSafejs.eval(rt, "get_user(1).name", callbacks)
    end

    test "callback returning list", %{rt: rt} do
      callbacks = %{
        "get_items" => fn [] -> {:ok, [1, 2, 3]} end
      }

      assert {:ok, 3} = ExSafejs.eval(rt, "get_items().length", callbacks)
    end

    test "callback called multiple times", %{rt: rt} do
      callbacks = %{
        "double" => fn [n] -> {:ok, n * 2} end
      }

      assert {:ok, 12} = ExSafejs.eval(rt, "double(2) + double(4)", callbacks)
    end

    test "multiple callbacks", %{rt: rt} do
      callbacks = %{
        "first" => fn [list] -> {:ok, List.first(list)} end,
        "last" => fn [list] -> {:ok, List.last(list)} end
      }

      code = """
      const arr = [10, 20, 30];
      first(arr) + last(arr);
      """

      assert {:ok, 40} = ExSafejs.eval(rt, code, callbacks)
    end

    test "callback error propagates as JS exception", %{rt: rt} do
      callbacks = %{
        "fail" => fn _args -> {:error, "something went wrong"} end
      }

      assert {:error, msg} = ExSafejs.eval(rt, "fail()", callbacks)
      assert msg =~ "something went wrong"
    end

    test "callback exception is caught", %{rt: rt} do
      callbacks = %{
        "blow_up" => fn _args -> raise "kaboom" end
      }

      assert {:error, msg} = ExSafejs.eval(rt, "blow_up()", callbacks)
      assert msg =~ "kaboom"
    end

    test "callbacks cleaned up after eval", %{rt: rt} do
      callbacks = %{"temp" => fn [] -> {:ok, "hi"} end}
      assert {:ok, "hi"} = ExSafejs.eval(rt, "temp()", callbacks)

      # temp should no longer exist
      assert {:error, _} = ExSafejs.eval(rt, "temp()")
    end

    test "normal eval works after callback eval", %{rt: rt} do
      callbacks = %{"inc" => fn [n] -> {:ok, n + 1} end}
      assert {:ok, 6} = ExSafejs.eval(rt, "inc(5)", callbacks)
      assert {:ok, 42} = ExSafejs.eval(rt, "21 * 2")
    end

    test "invalid callback return shape becomes error", %{rt: rt} do
      callbacks = %{"bad" => fn [] -> "bare string" end}
      assert {:error, msg} = ExSafejs.eval(rt, "bad()", callbacks)
      assert msg =~ "Invalid callback result"
    end

    test "callback arity mismatch gives clear error", %{rt: rt} do
      callbacks = %{"greet" => fn [name] -> {:ok, "Hi #{name}"} end}
      assert {:error, msg} = ExSafejs.eval(rt, "greet('a', 'b')", callbacks)
      assert msg =~ "Callback 'greet'"
      assert msg =~ "no matching clause for 2 arg(s)"
    end

    test "callback with wrong fun arity gives clear error", %{rt: rt} do
      # fn that takes two args instead of one (the args list)
      callbacks = %{"bad" => fn _a, _b -> {:ok, nil} end}
      assert {:error, msg} = ExSafejs.eval(rt, "bad(1)", callbacks)
      assert msg =~ "Callback 'bad'"
      assert msg =~ "function must accept a single argument"
    end

    test "callback returning nil", %{rt: rt} do
      callbacks = %{"nothing" => fn [] -> {:ok, nil} end}
      assert {:ok, nil} = ExSafejs.eval(rt, "nothing()", callbacks)
    end

    test "callback returning boolean", %{rt: rt} do
      callbacks = %{"check" => fn [n] -> {:ok, n > 0} end}
      assert {:ok, true} = ExSafejs.eval(rt, "check(5)", callbacks)
      assert {:ok, false} = ExSafejs.eval(rt, "check(-1)", callbacks)
    end

    test "callback returning nested structure", %{rt: rt} do
      callbacks = %{
        "data" => fn [] ->
          {:ok, %{"users" => [%{"name" => "Alice"}, %{"name" => "Bob"}]}}
        end
      }

      assert {:ok, "Bob"} = ExSafejs.eval(rt, "data().users[1].name", callbacks)
    end

    test "callback returning atom becomes string", %{rt: rt} do
      callbacks = %{"status" => fn [] -> {:ok, :active} end}
      assert {:ok, "active"} = ExSafejs.eval(rt, "status()", callbacks)
    end

    test "callback returning large integer", %{rt: rt} do
      callbacks = %{"big" => fn [] -> {:ok, 9_007_199_254_740_991} end}
      assert {:ok, 9_007_199_254_740_991} = ExSafejs.eval(rt, "big()", callbacks)
    end

    test "JS try/catch around failing callback", %{rt: rt} do
      callbacks = %{"risky" => fn [] -> {:error, "nope"} end}

      code = """
      try { risky(); } catch(e) { e.message || String(e); }
      """

      assert {:ok, "nope"} = ExSafejs.eval(rt, code, callbacks)
    end
  end

  describe "resource limits" do
    test "timeout" do
      {:ok, rt} = ExSafejs.start(timeout: 100)
      assert {:error, "timeout"} = ExSafejs.eval(rt, "while(true) {}")
      ExSafejs.stop(rt)
    end

    test "runtime usable after timeout" do
      {:ok, rt} = ExSafejs.start(timeout: 100)
      assert {:error, "timeout"} = ExSafejs.eval(rt, "while(true) {}")
      assert {:ok, 42} = ExSafejs.eval(rt, "21 * 2")
      ExSafejs.stop(rt)
    end

    test "timeout with callbacks" do
      {:ok, rt} = ExSafejs.start(timeout: 100)

      callbacks = %{
        "noop" => fn [] -> {:ok, nil} end
      }

      assert {:error, "timeout"} =
               ExSafejs.eval(rt, "noop(); while(true) {}", callbacks)

      assert {:ok, 1} = ExSafejs.eval(rt, "1")
      ExSafejs.stop(rt)
    end

    test "memory limit" do
      {:ok, rt} = ExSafejs.start(memory_limit: 256 * 1024)

      assert {:error, _msg} =
               ExSafejs.eval(rt, "const arr = []; while(true) { arr.push('x'.repeat(1000)); }")

      ExSafejs.stop(rt)
    end

    test "runtime usable after memory limit exceeded" do
      # Allocate inside a function so the memory is reclaimable once the OOM
      # unwinds. A top-level `const arr` global would pin the runtime at the
      # cap, and with quickjs-ng 0.15's stricter accounting every later
      # eval then legitimately OOMs too — that's the guest's fault, not a
      # recovery failure.
      {:ok, rt} = ExSafejs.start(memory_limit: 2 * 1024 * 1024)

      assert {:error, msg} =
               ExSafejs.eval(
                 rt,
                 "(function() { const arr = []; while(true) { arr.push('x'.repeat(1000)); } })()"
               )

      assert msg =~ "out of memory"
      assert {:ok, 1} = ExSafejs.eval(rt, "1")
      ExSafejs.stop(rt)
    end

    test "timeout during a pending promise job does not abort the VM" do
      # Regression test for a BEAM-killing SIGABRT (lpgauth/quicksand#2): with rquickjs
      # 0.11 the interrupt handler firing inside a pending job corrupted
      # refcounts, and freeing the runtime hit the `gc_decref_child`
      # assertion in quickjs.c — taking the whole node down. Fixed by
      # rquickjs 0.12 (upstream bug DelSkayn/rquickjs#663).
      {:ok, rt} = ExSafejs.start(timeout: 300)

      assert {:error, "timeout"} =
               ExSafejs.eval(rt, "Promise.resolve().then(() => { while(true) {} }); 1")

      # The abort fired on free; stopping cleanly is the regression check.
      :ok = ExSafejs.stop(rt)

      {:ok, rt2} = ExSafejs.start()
      assert {:ok, 42} = ExSafejs.eval(rt2, "40 + 2")
      ExSafejs.stop(rt2)
    end

    test "stack overflow" do
      {:ok, rt} = ExSafejs.start()
      assert {:error, msg} = ExSafejs.eval(rt, "function f() { return f(); } f()")
      assert is_binary(msg)
      ExSafejs.stop(rt)
    end

    test "runtime usable after stack overflow" do
      {:ok, rt} = ExSafejs.start()
      assert {:error, _} = ExSafejs.eval(rt, "function f() { return f(); } f()")
      assert {:ok, 1} = ExSafejs.eval(rt, "1")
      ExSafejs.stop(rt)
    end
  end

  describe "isolation" do
    test "separate runtimes are isolated" do
      {:ok, rt1} = ExSafejs.start()
      {:ok, rt2} = ExSafejs.start()

      ExSafejs.eval(rt1, "globalThis.shared = 'from_rt1'")
      assert {:error, _} = ExSafejs.eval(rt2, "shared")

      ExSafejs.stop(rt1)
      ExSafejs.stop(rt2)
    end
  end

  describe "protocol atoms" do
    setup do
      {:ok, rt} = ExSafejs.start()
      on_exit(fn -> ExSafejs.stop(rt) end)
      %{rt: rt}
    end

    test "ex_safejs_callback atom in callback messages", %{rt: rt} do
      callbacks = %{"ping" => fn [] -> {:ok, "pong"} end}
      assert {:ok, "pong"} = ExSafejs.eval(rt, "ping()", callbacks)
    end

    test "ex_safejs_result atom with ok", %{rt: rt} do
      callbacks = %{"id" => fn [x] -> {:ok, x} end}
      assert {:ok, 42} = ExSafejs.eval(rt, "id(42)", callbacks)
    end

    test "ex_safejs_result atom with error", %{rt: rt} do
      callbacks = %{"fail" => fn [] -> {:error, "nope"} end}
      assert {:error, msg} = ExSafejs.eval(rt, "fail()", callbacks)
      assert msg =~ "nope"
    end
  end

  describe "property-based: type round-trip" do
    setup do
      {:ok, rt} = ExSafejs.start()
      on_exit(fn -> ExSafejs.stop(rt) end)
      %{rt: rt}
    end

    # Generator for values that survive Elixir → JS → Elixir round-trip
    defp js_safe_value do
      tree(js_leaf(), fn leaf ->
        one_of([
          list_of(leaf, max_length: 5),
          map_of(string(:alphanumeric, min_length: 1, max_length: 8), leaf, max_length: 5)
        ])
      end)
    end

    defp js_leaf do
      one_of([
        constant(nil),
        boolean(),
        integer(-1_000_000..1_000_000),
        float(min: -1.0e6, max: 1.0e6),
        string(:alphanumeric, max_length: 50)
      ])
    end

    property "eval round-trips Elixir values through JS callbacks", %{rt: rt} do
      check all(value <- js_safe_value(), max_runs: 200) do
        callbacks = %{"echo" => fn [v] -> {:ok, v} end}
        {:ok, result} = ExSafejs.eval(rt, "echo(#{js_encode(value)})", callbacks)
        assert js_equal?(value, result)
      end
    end

    property "callback return values survive JS → Elixir", %{rt: rt} do
      check all(value <- js_safe_value(), max_runs: 200) do
        callbacks = %{"get" => fn [] -> {:ok, value} end}
        {:ok, result} = ExSafejs.eval(rt, "get()", callbacks)
        assert js_equal?(value, result)
      end
    end

    # Encode Elixir value as JS literal for eval
    defp js_encode(nil), do: "null"
    defp js_encode(true), do: "true"
    defp js_encode(false), do: "false"
    defp js_encode(n) when is_integer(n), do: Integer.to_string(n)

    defp js_encode(f) when is_float(f) do
      Float.to_string(f)
    end

    defp js_encode(s) when is_binary(s) do
      escaped =
        s
        |> String.replace("\\", "\\\\")
        |> String.replace("\"", "\\\"")
        |> String.replace("\n", "\\n")
        |> String.replace("\r", "\\r")

      "\"#{escaped}\""
    end

    defp js_encode(list) when is_list(list) do
      "[#{Enum.map_join(list, ",", &js_encode/1)}]"
    end

    defp js_encode(map) when is_map(map) do
      pairs = Enum.map_join(map, ",", fn {k, v} -> "#{js_encode(k)}:#{js_encode(v)}" end)
      "({#{pairs}})"
    end

    # Compare values accounting for JS type coercion:
    # - JS doesn't distinguish int/float, so 1.0 == 1
    # - NaN/Infinity become nil
    defp js_equal?(a, b) when is_float(a) and is_integer(b) do
      Float.round(a, 0) == a and trunc(a) == b
    end

    defp js_equal?(a, b) when is_integer(a) and is_float(b) do
      Float.round(b, 0) == b and a == trunc(b)
    end

    defp js_equal?(a, b) when is_float(a) and is_float(b) do
      abs(a - b) < 1.0e-9
    end

    defp js_equal?(a, b) when is_list(a) and is_list(b) do
      length(a) == length(b) and Enum.all?(Enum.zip(a, b), fn {x, y} -> js_equal?(x, y) end)
    end

    defp js_equal?(a, b) when is_map(a) and is_map(b) do
      Map.keys(a) == Map.keys(b) and
        Enum.all?(Map.keys(a), fn k -> js_equal?(Map.get(a, k), Map.get(b, k)) end)
    end

    defp js_equal?(a, b), do: a === b
  end
end
