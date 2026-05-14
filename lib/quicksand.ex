defmodule Quicksand do
  @moduledoc """
  Sandboxed JavaScript execution via QuickJS-NG.

  Provides `eval/2` for simple evaluation and `eval/3` for evaluation
  with pre-registered Elixir callback functions callable from JS.
  Each runtime runs on a dedicated OS thread with configurable
  memory and time limits.

  ## Reserved JS globals

  When using `eval/3` in callback mode, quicksand installs four
  `globalThis.__quicksand_*` properties on the JS runtime:
  `__quicksand_make_wrapper`, `__quicksand_dispatch`,
  `__quicksand_cb_args`, `__quicksand_cb_result`. User JavaScript
  must not overwrite these or the callback machinery silently
  breaks for that runtime. See `README.md` for the full list and
  rationale.
  """

  @default_timeout 30_000
  @default_memory_limit 256 * 1024 * 1024
  @default_max_stack_size 1024 * 1024

  @typep runtime :: reference()
  @type js_result :: {:ok, term()} | {:error, String.t()}

  @doc """
  Start a new JavaScript runtime on a dedicated OS thread.

  ## Options

    * `:timeout` — max eval time in milliseconds (default `30_000`)
    * `:memory_limit` — max JS heap in bytes (default `268_435_456`)
    * `:max_stack_size` — max JS stack in bytes (default `1_048_576`)

  """
  @spec start(keyword()) :: {:ok, runtime()} | {:error, term()}
  def start(opts \\ []) do
    timeout = Keyword.get(opts, :timeout, @default_timeout)
    memory_limit = Keyword.get(opts, :memory_limit, @default_memory_limit)
    max_stack_size = Keyword.get(opts, :max_stack_size, @default_max_stack_size)

    ref = make_ref()
    Quicksand.Native.start_runtime(ref, timeout, memory_limit, max_stack_size)

    receive do
      {:quicksand_start, ^ref, {:ok, resource}} -> {:ok, resource}
      {:quicksand_start, ^ref, {:error, reason}} -> {:error, reason}
    after
      5_000 -> {:error, :start_timeout}
    end
  end

  @doc """
  Evaluate JavaScript code and return the result.

      {:ok, 3} = Quicksand.eval(rt, "1 + 2")

  """
  @spec eval(runtime(), String.t()) :: js_result()
  def eval(runtime, code) do
    Quicksand.Native.eval_sync(runtime, code)
  end

  @doc """
  Evaluate JavaScript code with pre-registered Elixir callbacks.

  Each callback receives its arguments as a list and must return
  `{:ok, value}` or `{:error, reason}`. Errors become JS exceptions.

      callbacks = %{
        "add" => fn [a, b] -> {:ok, a + b} end
      }
      {:ok, 5} = Quicksand.eval(rt, "add(2, 3)", callbacks)

  """
  @spec eval(runtime(), String.t(), map()) :: js_result()
  def eval(runtime, code, callbacks) when is_map(callbacks) do
    fn_names = Map.keys(callbacks)

    case Quicksand.Native.eval_with_callbacks(runtime, code, fn_names) do
      {:ok, _} ->
        timeout = Quicksand.Native.get_timeout(runtime)
        callback_loop(runtime, callbacks, timeout)

      {:error, _} = error ->
        error
    end
  end

  @doc "Check if a runtime is alive."
  @spec alive?(runtime()) :: boolean()
  def alive?(runtime) do
    Quicksand.Native.is_alive(runtime)
  catch
    :error, :badarg -> false
  end

  @doc "Stop a runtime. Idempotent — safe to call multiple times."
  @spec stop(runtime()) :: :ok
  def stop(runtime) do
    Quicksand.Native.stop_runtime(runtime)
  catch
    :error, :badarg -> :ok
  end

  defp callback_loop(runtime, callbacks, timeout) do
    receive do
      {:quicksand_result, {:ok, value}} ->
        {:ok, value}

      {:quicksand_result, {:error, reason}} ->
        {:error, reason}

      {:quicksand_callback, id, name, args} ->
        result =
          case Map.fetch(callbacks, name) do
            {:ok, fun} ->
              try do
                fun.(args)
              rescue
                _e in [FunctionClauseError] ->
                  {:error, "Callback '#{name}': no matching clause for #{length(args)} arg(s)"}

                _e in [BadArityError] ->
                  {:error,
                   "Callback '#{name}': function must accept a single argument (the args list)"}

                e ->
                  {:error, Exception.message(e)}
              end

            :error ->
              {:error, "Unknown callback: #{name}"}
          end

        result = validate_callback_result(result)
        Quicksand.Native.respond_callback(runtime, id, result)
        callback_loop(runtime, callbacks, timeout)
    after
      timeout ->
        Quicksand.Native.interrupt(runtime)
        {:error, "timeout"}
    end
  end

  defp validate_callback_result({:ok, _} = result), do: result
  defp validate_callback_result({:error, _} = result), do: result
  defp validate_callback_result(other), do: {:error, "Invalid callback result: #{inspect(other)}"}
end
