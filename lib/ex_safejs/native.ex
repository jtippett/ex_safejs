defmodule ExSafejs.Native do
  @moduledoc false

  version = Mix.Project.config()[:version]

  use RustlerPrecompiled,
    otp_app: :ex_safejs,
    crate: "ex_safejs",
    base_url: "https://github.com/jtippett/ex_safejs/releases/download/v#{version}",
    force_build: System.get_env("EX_SAFEJS_BUILD") in ["1", "true"],
    targets: ~w(
      aarch64-apple-darwin
      aarch64-unknown-linux-gnu
      x86_64-apple-darwin
      x86_64-unknown-linux-gnu
    ),
    nif_versions: ["2.15"],
    version: version

  def start_runtime(_ref, _timeout_ms, _memory_limit, _max_stack_size, _gc_threshold),
    do: :erlang.nif_error(:nif_not_loaded)

  def eval_start(_resource, _eval_id, _code, _fn_names),
    do: :erlang.nif_error(:nif_not_loaded)

  def respond_callback(_resource, _callback_id, _result),
    do: :erlang.nif_error(:nif_not_loaded)

  def get_timeout(_resource),
    do: :erlang.nif_error(:nif_not_loaded)

  def is_alive(_resource),
    do: :erlang.nif_error(:nif_not_loaded)

  def interrupt(_resource, _eval_id),
    do: :erlang.nif_error(:nif_not_loaded)

  def stop_runtime(_resource),
    do: :erlang.nif_error(:nif_not_loaded)
end
