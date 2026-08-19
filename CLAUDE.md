# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Build & Test Commands

```bash
EX_SAFEJS_BUILD=true mix deps.get  # fetch deps + force local Rust build
EX_SAFEJS_BUILD=true mix compile   # build (includes Rust NIF compilation)
EX_SAFEJS_BUILD=true mix test      # run all tests
mix test test/ex_safejs_test.exs:42  # run single test by line number
mix compile --warnings-as-errors   # build with strict warnings
mix format                         # format Elixir code
mix format --check-formatted       # check Elixir formatting
mix dialyzer                       # static type analysis
cargo fmt                          # format Rust code
cargo fmt --check                  # check Rust formatting
cargo clippy -- -D warnings        # Rust linter
```

`EX_SAFEJS_BUILD=true` is required for local development to force compilation from Rust source instead of downloading precompiled binaries. Without it, `RustlerPrecompiled` will try to fetch binaries from GitHub releases.

## Releasing

```bash
./scripts/release.sh  # tags, pushes, waits for CI, generates checksums
```

The script reads the version from `mix.exs`, creates a git tag, waits for the release workflow to build precompiled NIFs for all targets, then generates checksums. After it completes, commit the checksum file and run `mix hex.publish`.

## Architecture

ExSafejs is an Elixir NIF wrapping QuickJS-NG (via `rquickjs` crate) for sandboxed JS execution. Resource-only API (no GenServer).

### Thread Model

Each runtime spawns a dedicated OS thread running a QuickJS worker. Communication happens via `mpsc` channels. The BEAM process never blocks on JS execution directly, and the Elixir side owns all timing.

```
BEAM Process (ExSafejs.eval/3)
  ├─ eval_start NIF ──► mpsc channel ──► Worker Thread
  │    (returns :ok           Eval{code, fn_names, caller, eval_id}
  │     immediately)          clears interrupt, installs callback Functions,
  │                           evals; a promise completion value is DRIVEN:
  │                           drain jobs → check state → settle (loop);
  │                           pending + empty queue = deadlock error
  ├─ receive {:ex_safejs_callback, eval_id, cb_id, name, args}
  │    └─► run Elixir fun (wall time excluded from the JS deadline)
  │        └─► respond_callback NIF ──► CallbackRegistry channel ──► dispatch resumes
  ├─ receive {:ex_safejs_result, eval_id, {:ok, v} | {:error, {kind, msg}}}
  └─ deadline expiry: interrupt NIF, then ABSORB the late result/callback
     messages (grace 2s) so no straggler leaks into the caller's mailbox
```

### Callback Mechanism

- Each Elixir callback is installed as a real JS `Function` whose Rust closure captures the callback name, caller pid, eval id, and registry (`make_dispatch` in worker.rs — an `impl for<'js> Fn` so it can return a `Value<'js>`). There are **no reserved globals**; nothing the guest can read or write is on the dispatch path.
- Dispatch sends `{:ex_safejs_callback, eval_id, cb_id, name, args}` and blocks on the registry channel with **no Rust-side timeout** — Elixir owns timing, and `close()` on stop/Drop disconnects the channel to unblock a stranded dispatch.
- A callback returning `{:error, reason}` throws a catchable JS `Error` with `reason` verbatim. A callback that **raises** is sanitized: the guest sees `"host function failed"`, and the real exception rides an Elixir-side channel back to the caller as a `:host_error` if the guest doesn't catch it.

### Type Conversion

Two-phase for Elixir→JS (needed because NIF thread has `Env` access but not `rquickjs::Ctx`, worker thread has the reverse):
1. NIF thread: `Term` → `TermValue` (intermediate enum, `convert.rs:term_to_intermediate`)
2. Worker thread: `TermValue` → `rquickjs::Value` (`convert.rs:intermediate_to_js`)

JS→Elixir is single-phase: `rquickjs::Value` → `JsValue` enum → Erlang `Term` (via `Encoder` impl).

### Interrupt / Timeout

The `interrupt` `Arc<AtomicBool>` is shared between the NIF side and the worker. It is always cleared on the **worker side** when starting a new eval (not the NIF side) to avoid a race where the flag is cleared before a previous timed-out eval has been interrupted. The QuickJS interrupt handler checks this flag on every loop iteration.

The deadline lives entirely in Elixir (`ExSafejs.await/6`): host-callback wall time is added back to the deadline, so `:timeout` means JS compute budget. Only the deadline (or stop/Drop) sets the interrupt, so a fired interrupt is always honestly classified as `:timeout`.

### Key Files

- `lib/ex_safejs.ex` — public API, callback receive loop, validation
- `lib/ex_safejs/native.ex` — RustlerPrecompiled NIF stubs (set `EX_SAFEJS_BUILD=true` to compile from source)
- `native/ex_safejs/src/lib.rs` — NIF entry points
- `native/ex_safejs/src/worker.rs` — worker thread, QuickJS eval, callback dispatch
- `native/ex_safejs/src/convert.rs` — bidirectional type conversion (JS ↔ Erlang)
- `native/ex_safejs/src/runtime.rs` — Runtime resource, CallbackRegistry, `send_to_pid`
