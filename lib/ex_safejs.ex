defmodule ExSafejs do
  @moduledoc """
  Sandboxed JavaScript execution via QuickJS-NG.

  `eval/2` and `eval/3` are **async-aware**: if the completion value of the
  guest code is a promise (an `async` arrow, a `.then` chain, ...), the
  runtime drives the job queue until it settles and returns the settled
  value — a rejection comes back as a structured error. Synchronous code
  returns directly, as before.

  Each runtime runs on a dedicated OS thread with a hard memory cap, a stack
  limit, and a JS compute deadline. The deadline covers JS execution only:
  time spent inside Elixir callbacks is excluded, so a slow host call never
  reads as guest misbehavior.

  Failures are `{:error, %ExSafejs.Error{}}` with a `:kind` callers can
  branch on — see `ExSafejs.Error`.

  Elixir callbacks are installed as real JS functions whose dispatch path is
  captured host-side; guest code may shadow or delete the global binding but
  that only loses its own access. There are no reserved `__*` globals.
  """

  alias ExSafejs.Error

  @default_timeout 30_000
  @default_memory_limit 256 * 1024 * 1024
  @default_max_stack_size 1024 * 1024
  @default_gc_threshold 4 * 1024 * 1024

  # The exact guest-visible message for a raised (not returned-as-error)
  # callback. The real exception is delivered to the Elixir caller instead;
  # untrusted JS learns nothing from a host raise beyond "it failed".
  @host_failed "host function failed"

  # Grace period for the worker to acknowledge an interrupt after the
  # deadline fires. The interrupted eval errors out promptly, so its result
  # message is absorbed here instead of leaking into the caller's mailbox.
  @interrupt_grace 2_000

  @typep runtime :: reference()
  @type js_result :: {:ok, term()} | {:error, Error.t()}

  @doc """
  Start a new JavaScript runtime on a dedicated OS thread.

  ## Options

    * `:timeout` — max JS compute time per eval in milliseconds, excluding
      time spent in Elixir callbacks (default `30_000`)
    * `:memory_limit` — max JS heap in bytes (default `268_435_456`)
    * `:max_stack_size` — max JS stack in bytes (default `1_048_576`)
    * `:gc_threshold` — GC trigger threshold in bytes (default `4_194_304`)

  """
  @spec start(keyword()) :: {:ok, runtime()} | {:error, term()}
  def start(opts \\ []) do
    timeout = Keyword.get(opts, :timeout, @default_timeout)
    memory_limit = Keyword.get(opts, :memory_limit, @default_memory_limit)
    max_stack_size = Keyword.get(opts, :max_stack_size, @default_max_stack_size)
    gc_threshold = Keyword.get(opts, :gc_threshold, @default_gc_threshold)

    ref = make_ref()
    ExSafejs.Native.start_runtime(ref, timeout, memory_limit, max_stack_size, gc_threshold)

    receive do
      {:ex_safejs_start, ^ref, {:ok, resource}} -> {:ok, resource}
      {:ex_safejs_start, ^ref, {:error, reason}} -> {:error, reason}
    after
      5_000 -> {:error, :start_timeout}
    end
  end

  @doc """
  Evaluate JavaScript code and return the result.

      {:ok, 3} = ExSafejs.eval(rt, "1 + 2")
      {:ok, 3} = ExSafejs.eval(rt, "(async () => 1 + 2)()")

  """
  @spec eval(runtime(), String.t()) :: js_result()
  def eval(runtime, code), do: eval(runtime, code, %{})

  @doc """
  Evaluate JavaScript code with pre-registered Elixir callbacks.

  Each callback receives its arguments as a list and must return
  `{:ok, value}` or `{:error, reason}` with a binary reason. A returned
  `{:error, reason}` becomes a catchable JS exception carrying `reason`
  verbatim — never put a secret in it. A *raised* exception instead
  surfaces to the guest as a generic `"host function failed"` exception; if
  the guest doesn't catch it, the eval returns a `:host_error` carrying the
  real exception message to the Elixir caller.

      callbacks = %{"add" => fn [a, b] -> {:ok, a + b} end}
      {:ok, 5} = ExSafejs.eval(rt, "add(2, 3)", callbacks)

  Callbacks work under `await` too — a blocking host call is simply a value
  by the time the guest sees it:

      {:ok, 5} = ExSafejs.eval(rt, "(async () => await add(2, 3))()", callbacks)

  """
  @spec eval(runtime(), String.t(), map()) :: js_result()
  def eval(runtime, code, callbacks) when is_map(callbacks) do
    eval_id = System.unique_integer([:positive])
    fn_names = Map.keys(callbacks)

    case ExSafejs.Native.eval_start(runtime, eval_id, code, fn_names) do
      :ok ->
        timeout = ExSafejs.Native.get_timeout(runtime)
        deadline = System.monotonic_time(:millisecond) + timeout
        await(runtime, eval_id, callbacks, deadline, timeout, nil)

      :dead_runtime ->
        {:error, %Error{kind: :dead_runtime, message: "runtime is stopped"}}
    end
  catch
    :error, :badarg ->
      {:error, %Error{kind: :dead_runtime, message: "runtime is stopped"}}
  end

  @doc "Check if a runtime is alive."
  @spec alive?(runtime()) :: boolean()
  def alive?(runtime) do
    ExSafejs.Native.is_alive(runtime)
  catch
    :error, :badarg -> false
  end

  @doc "Stop a runtime. Idempotent — safe to call multiple times."
  @spec stop(runtime()) :: :ok
  def stop(runtime) do
    ExSafejs.Native.stop_runtime(runtime)
  catch
    :error, :badarg -> :ok
  end

  # ── eval driving ──────────────────────────────────────────────────────────

  defp await(runtime, eval_id, callbacks, deadline, timeout, host_exc) do
    remaining = max(deadline - System.monotonic_time(:millisecond), 0)

    receive do
      {:ex_safejs_result, ^eval_id, result} ->
        finalize(result, host_exc, timeout)

      {:ex_safejs_callback, ^eval_id, cb_id, name, args} ->
        started = System.monotonic_time(:millisecond)
        {reply, host_exc} = run_callback(callbacks, name, args, host_exc)
        ExSafejs.Native.respond_callback(runtime, cb_id, reply)
        elapsed = System.monotonic_time(:millisecond) - started
        # Host-call wall time doesn't count against the JS budget.
        await(runtime, eval_id, callbacks, deadline + elapsed, timeout, host_exc)
    after
      remaining ->
        ExSafejs.Native.interrupt(runtime)
        absorb(runtime, eval_id, timeout)
    end
  end

  # After an interrupt, wait for the worker's (now-erroring) eval to send its
  # result and absorb it, so no straggler is left in the caller's mailbox. A
  # callback message racing the deadline is answered with an error so the
  # worker unblocks and can observe the interrupt.
  defp absorb(runtime, eval_id, timeout) do
    timeout_error = {:error, %Error{kind: :timeout, message: "timeout after #{timeout}ms"}}

    receive do
      {:ex_safejs_result, ^eval_id, _late_result} ->
        timeout_error

      {:ex_safejs_callback, ^eval_id, cb_id, _name, _args} ->
        ExSafejs.Native.respond_callback(runtime, cb_id, {:error, "timeout"})
        absorb(runtime, eval_id, timeout)
    after
      @interrupt_grace ->
        timeout_error
    end
  end

  defp run_callback(callbacks, name, args, host_exc) do
    case Map.fetch(callbacks, name) do
      {:ok, fun} ->
        try do
          case fun.(args) do
            {:ok, _} = ok ->
              {ok, host_exc}

            {:error, reason} when is_binary(reason) ->
              {{:error, reason}, host_exc}

            other ->
              # A wrong-shape result is a host bug; don't inspect() host data
              # into the guest — deliver the detail via the side channel.
              detail = %RuntimeError{
                message: "callback #{inspect(name)} returned an invalid result: #{inspect(other)}"
              }

              {{:error, @host_failed}, host_exc || detail}
          end
        rescue
          e ->
            # Sanitized to the guest; the real exception rides the side
            # channel to the Elixir caller (first raise wins).
            detail = %RuntimeError{
              message: "callback #{inspect(name)} raised: #{Exception.message(e)}"
            }

            {{:error, @host_failed}, host_exc || detail}
        end

      :error ->
        {{:error, "Unknown callback: #{name}"}, host_exc}
    end
  end

  defp finalize({:ok, value}, _host_exc, _timeout), do: {:ok, value}

  defp finalize({:error, {kind, message}}, host_exc, timeout) do
    {first, stack} = split_message(message)

    error =
      case kind do
        :timeout -> %Error{kind: :timeout, message: "timeout after #{timeout}ms"}
        :deadlock -> %Error{kind: :deadlock, message: first, stack: stack}
        :js_error -> classify_js(first, stack)
      end

    restore_host_exception(error, host_exc)
  end

  defp classify_js(first, stack) do
    cond do
      String.starts_with?(first, "out of memory") ->
        %Error{kind: :memory_limit, message: first, stack: stack}

      first =~ "Maximum call stack size exceeded" or first =~ "stack overflow" ->
        %Error{kind: :stack_overflow, message: first, stack: stack}

      true ->
        %Error{kind: :js_error, message: first, stack: stack}
    end
  end

  # A raised callback whose sanitized exception the guest did NOT catch
  # bubbles out with the fixed message; swap the real exception back in for
  # the Elixir caller. If the guest caught it and failed some other way, its
  # own error stands (a guest-authored error that merely quotes the sentinel
  # text can't be told apart — same residual ambiguity as any
  # message-matching scheme).
  defp restore_host_exception(%Error{kind: :js_error, message: @host_failed}, exc)
       when not is_nil(exc) do
    {:error, %Error{kind: :host_error, message: Exception.message(exc)}}
  end

  defp restore_host_exception(error, _host_exc), do: {:error, error}

  defp split_message(message) do
    case String.split(message, "\n", parts: 2) do
      [first] -> {first, nil}
      [first, ""] -> {first, nil}
      [first, rest] -> {first, rest}
    end
  end
end
