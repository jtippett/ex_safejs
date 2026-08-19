defmodule ExSafejs.SecurityPocTest do
  @moduledoc """
  Adversarial PoCs for the security review. Each test is bounded so it
  cannot harm the host: small allocations, short deadlines, everything
  stopped at the end.
  """
  use ExUnit.Case

  alias ExSafejs.Error

  defp ms, do: System.monotonic_time(:millisecond)

  test "attack surface inventory: what globals exist" do
    {:ok, rt} = ExSafejs.start()

    code = """
    [typeof globalThis, typeof Function, typeof eval, typeof SharedArrayBuffer,
     typeof Atomics, typeof WebAssembly, typeof std, typeof os, typeof print,
     typeof require, typeof process, typeof global]
    """

    assert {:ok, surface} = ExSafejs.eval(rt, code)
    IO.inspect(surface, label: "GLOBAL SURFACE")
    ExSafejs.stop(rt)
  end

  test "PoC A: regexp backtracking vs interrupt" do
    {:ok, rt} = ExSafejs.start(timeout: 500)
    # 28 a's: ~2^28 backtracks if libregexp is exponential. Bounded.
    payload = String.duplicate("a", 28) <> "b"
    started = ms()
    result = ExSafejs.eval(rt, ~s[/^(a+)+$/.test("#{payload}")])
    elapsed = ms() - started
    IO.inspect({result, elapsed}, label: "REDOS (deadline 500ms)")
    ExSafejs.stop(rt)
  end

  test "PoC B: sparse array length escapes memory cap and deadline" do
    {:ok, rt} = ExSafejs.start(timeout: 300, memory_limit: 64 * 1024 * 1024)

    # Baseline: with a huge deadline, how long does converting a 20M-hole
    # array take, and does it succeed despite the JS heap staying tiny?
    {:ok, rt2} = ExSafejs.start(timeout: 120_000, memory_limit: 64 * 1024 * 1024)
    started = ms()
    r = ExSafejs.eval(rt2, "const a = []; a.length = 20_000_000; a")
    conv_ms = ms() - started
    IO.inspect({elem(r, 0), conv_ms}, label: "20M-hole array, no deadline (conversion wall time)")

    # Now with a 300ms deadline: JS finishes in microseconds, conversion is
    # uninterruptible. Result should be :timeout even though guest did nothing
    # wrong, and the caller is stuck for deadline + 2s grace.
    started = ms()
    r2 = ExSafejs.eval(rt, "const a = []; a.length = 20_000_000; a")
    IO.inspect({r2, ms() - started}, label: "20M-hole array, 300ms deadline")

    # And the runtime is still busy converting: a trivial eval queued behind
    # it also dies to the deadline.
    started = ms()
    r3 = ExSafejs.eval(rt, "1")
    IO.inspect({r3, ms() - started}, label: "follow-up eval while worker converts")

    ExSafejs.stop(rt)
    ExSafejs.stop(rt2)
  end

  test "PoC C: BigInt result parse is uninterruptible CPU" do
    {:ok, rt} = ExSafejs.start(timeout: 300)
    started = ms()
    # 1M decimal digits, ~0.5MB of JS heap. num-bigint parses base-10.
    r = ExSafejs.eval(rt, "10n ** 1_000_000n")
    IO.inspect({elem(r, 0), ms() - started}, label: "1M-digit BigInt, 300ms deadline")
    ExSafejs.stop(rt)
  end

  test "PoC D: saved callback reference wedges the runtime across evals" do
    {:ok, rt} = ExSafejs.start(timeout: 300)
    ping = fn [] -> {:ok, "pong"} end

    assert {:ok, _} = ExSafejs.eval(rt, "globalThis._saved = ping; 'ok'", %{"ping" => ping})

    started = ms()
    r = ExSafejs.eval(rt, "_saved()")
    IO.inspect({r, ms() - started}, label: "stale callback call")

    # Worker is now blocked in rx.recv() forever (nobody will ever respond):
    # a trivial eval queued behind it must also time out.
    started = ms()
    r2 = ExSafejs.eval(rt, "1")
    IO.inspect({r2, ms() - started}, label: "eval behind wedge")

    # stop() closes the registry -> dispatch errors -> worker unwinds.
    :ok = ExSafejs.stop(rt)
    {:ok, rt3} = ExSafejs.start()
    assert {:ok, 1} = ExSafejs.eval(rt3, "1")
    ExSafejs.stop(rt3)
    flush_protocol_messages()
  end

  defp flush_protocol_messages do
    receive do
      {:ex_safejs_callback, _, _, _, _} -> flush_protocol_messages()
      {:ex_safejs_result, _, _} -> flush_protocol_messages()
    after
      0 -> :ok
    end
  end

  test "PoC E: shared runtime = head-of-line blocking (queued eval burns deadline waiting)" do
    {:ok, rt} = ExSafejs.start(timeout: 10_000)
    test_pid = self()

    # Tenant A: a few seconds of legal compute, well within the 10s budget.
    a =
      Task.async(fn ->
        r = ExSafejs.eval(rt, "let s = 0; for (let i = 0; i < 2e9; i++) { s += i; } s")
        send(test_pid, {:a_done, r})
      end)

    # Give A a moment to reach the worker.
    Process.sleep(200)

    # Tenant B: trivial eval. It must wait for A; the wait counts against
    # B's wall-clock deadline even though B gets zero JS compute.
    started = ms()
    r_b = ExSafejs.eval(rt, "1")
    IO.inspect({r_b, ms() - started}, label: "tenant B trivial eval queued behind A")

    receive do
      {:a_done, r_a} -> IO.inspect(r_a, label: "tenant A")
    after
      15_000 -> flunk("tenant A never finished")
    end

    Task.shutdown(a, :brutal_kill)
    ExSafejs.stop(rt)
  end

  test "PoC F: TypedArray result amplification" do
    {:ok, rt} = ExSafejs.start(timeout: 30_000)
    started = ms()
    r = ExSafejs.eval(rt, "new Uint8Array(5_000_000)")
    elapsed = ms() - started

    case r do
      {:ok, m} when is_map(m) -> IO.inspect({map_size(m), elapsed}, label: "Uint8Array 5MB ->")
      other -> IO.inspect({other, elapsed}, label: "Uint8Array 5MB ->")
    end

    ExSafejs.stop(rt)
  end

  test "PoC G: Atomics.wait blocks the worker uninterruptibly" do
    {:ok, rt} = ExSafejs.start(timeout: 300)

    case ExSafejs.eval(rt, "typeof Atomics") do
      {:ok, "object"} ->
        started = ms()

        r =
          ExSafejs.eval(
            rt,
            "const sab = new SharedArrayBuffer(4); Atomics.wait(new Int32Array(sab), 0, 0); 'woke'"
          )

        IO.inspect({r, ms() - started}, label: "Atomics.wait (300ms deadline)")

        # Can stop() even recover this one? Worker is parked in a futex.
        started = ms()
        ExSafejs.stop(rt)
        IO.inspect(ms() - started, label: "stop() took")

      {:ok, other} ->
        IO.inspect(other, label: "Atomics not exposed")
        ExSafejs.stop(rt)
    end
  end

  test "PoC H: guest forges error-kind classification strings" do
    {:ok, rt} = ExSafejs.start()
    r = ExSafejs.eval(rt, ~s[throw new Error("out of memory: just kidding")])
    IO.inspect(r, label: "forged OOM")
    r2 = ExSafejs.eval(rt, ~s[throw new Error("Maximum call stack size exceeded lol")])
    IO.inspect(r2, label: "forged stack overflow")
    ExSafejs.stop(rt)
  end
end
