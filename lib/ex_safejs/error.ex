defmodule ExSafejs.Error do
  @moduledoc """
  Structured sandbox error.

  Every failed eval returns `{:error, %ExSafejs.Error{}}` with a `:kind` a
  caller (or an LLM repair loop) can branch on:

    * `:timeout` — the guest exceeded its JS compute budget. Host-callback
      wall time does not count against it.
    * `:deadlock` — the top-level promise can never settle: it is pending,
      the job queue is empty, and no host call is in flight (e.g.
      `new Promise(() => {})`).
    * `:memory_limit` — the hard heap cap was hit.
    * `:stack_overflow` — the stack limit was hit (catchable; the runtime
      survives).
    * `:js_error` — the guest threw or the code failed to parse.
    * `:host_error` — an Elixir callback raised. The guest saw only a
      generic `"host function failed"` exception; `message` here carries
      the real exception message for the host caller.
    * `:dead_runtime` — eval on a stopped runtime.
    * `:start_failed` — the runtime could not be created.

  `message` is the first line of the underlying report; `stack` carries the
  JS stack trace when the engine provided one.

  Kind classification for `:memory_limit`/`:stack_overflow` matches on
  QuickJS-NG message strings — an upstream contract pinned by tests.
  """

  defexception [:kind, :message, :stack]

  @type kind ::
          :timeout
          | :deadlock
          | :memory_limit
          | :stack_overflow
          | :js_error
          | :host_error
          | :dead_runtime
          | :start_failed

  @type t :: %__MODULE__{kind: kind(), message: String.t(), stack: String.t() | nil}
end
