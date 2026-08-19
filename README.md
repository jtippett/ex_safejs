# ExSafejs

Sandboxed JavaScript execution for Elixir via [QuickJS-NG](https://github.com/quickjs-ng/quickjs).

ExSafejs embeds the QuickJS-NG engine as a Rustler NIF, giving you in-process JS evaluation with strict memory and time limits. Each runtime runs on a dedicated OS thread — JS execution never blocks BEAM schedulers.

ExSafejs is a hard fork of [lpgauth/quicksand](https://github.com/lpgauth/quicksand)
(MIT), started to ship the rquickjs 0.12 fix for a BEAM-killing SIGABRT on
timeout-during-a-pending-promise-job
([quicksand#2](https://github.com/lpgauth/quicksand/issues/2) /
[PR #3](https://github.com/lpgauth/quicksand/pull/3)) and to evolve the API
independently from there.

## What the limits do — and don't — claim

The memory limit, stack limit, and timeout are **resource guards**: they
reliably contain allocation bombs, runaway recursion, and infinite loops in
untrusted JS, turning each into a structured error with the runtime (and the
BEAM) surviving. What a NIF embedding cannot claim is **host-memory
isolation**: the engine shares the BEAM's address space, so a hypothetical
memory-corruption exploit *in QuickJS-NG itself* is not contained by this
library. If your threat model includes engine-exploit-grade adversaries,
run the evaluating node as a disposable OS process rather than inside your
main application VM. (This distinction is borrowed from the security
assessment of langchain-ai/quickjs-rs, which audited exactly this
architecture.)

## Features

- Sandboxed JS with no filesystem, network, or OS access
- **Async-aware eval**: async arrows, `await`, `.then` chains and
  `Promise.all` are driven to settlement; a never-settling promise is
  detected as a `:deadlock` error immediately instead of burning the timeout
- Configurable memory limit, execution timeout, stack size, and GC threshold
- The timeout is a **JS compute budget**: time spent inside Elixir callbacks
  is excluded, so a slow host call never reads as guest misbehavior
- Structured errors (`%ExSafejs.Error{kind: ...}`) a caller — or an LLM
  repair loop — can branch on
- Pre-registered Elixir callbacks callable from JS, installed as real
  function objects with the dispatch path captured host-side (no reserved
  globals, nothing the guest writes is on the trust path)
- Raised callback exceptions are sanitized to the guest (`"host function
  failed"`) while the real exception reaches the Elixir caller
- Direct Erlang term <-> JS value conversion (no JSON serialization)
- Resource-only API (no GenServer overhead)

## Requirements

- Elixir >= 1.15
- Precompiled NIF binaries are provided for macOS (ARM/Intel) and Linux (x86/ARM)
- Rust toolchain only needed if building from source

## Installation

Add to your `mix.exs`:

```elixir
def deps do
  [
    {:ex_safejs, "~> 0.3.0"}
  ]
end
```

Precompiled binaries will be downloaded automatically. To build from source instead:

```bash
EX_SAFEJS_BUILD=true mix deps.compile ex_safejs
```

## Usage

### Basic Evaluation

```elixir
{:ok, rt} = ExSafejs.start()

{:ok, 3} = ExSafejs.eval(rt, "1 + 2")
{:ok, "hello"} = ExSafejs.eval(rt, "'hello'")
{:ok, %{"a" => 1}} = ExSafejs.eval(rt, "({a: 1})")

# Async code settles before returning
{:ok, 3} = ExSafejs.eval(rt, "(async () => 1 + 2)()")
{:ok, 2} = ExSafejs.eval(rt, "Promise.resolve(1).then(x => x + 1)")

# A promise nothing can settle is reported immediately
{:error, %ExSafejs.Error{kind: :deadlock}} = ExSafejs.eval(rt, "new Promise(() => {})")

:ok = ExSafejs.stop(rt)
```

### Resource Limits

```elixir
{:ok, rt} = ExSafejs.start(
  timeout: 5_000,            # 5 seconds max execution time
  memory_limit: 10_000_000,  # ~10 MB heap limit
  max_stack_size: 512_000    # 512 KB stack
)

# Infinite loops are interrupted
{:error, %ExSafejs.Error{kind: :timeout}} = ExSafejs.eval(rt, "while(true) {}")

# Runtime remains usable after timeout
{:ok, 42} = ExSafejs.eval(rt, "42")
```

### Callbacks

Register Elixir functions that JS code can call synchronously:

```elixir
{:ok, rt} = ExSafejs.start()

callbacks = %{
  "fetch_user" => fn [id] ->
    user = MyApp.Repo.get!(User, id)
    {:ok, %{"name" => user.name, "email" => user.email}}
  end,
  "log" => fn [message] ->
    Logger.info("JS: #{message}")
    {:ok, nil}
  end
}

{:ok, "Alice"} = ExSafejs.eval(rt, """
  const user = fetch_user(1);
  log("Found user: " + user.name);
  user.name;
""", callbacks)
```

Callbacks work under `await` too — the host call blocks the JS thread and
is a plain value by the time the guest sees it, so `await fetch_user(1)` and
`Promise.all([fetch_user(1), fetch_user(2)])` both work (`Promise.all` runs
the calls serially):

```elixir
{:ok, "Alice"} = ExSafejs.eval(rt, "(async () => (await fetch_user(1)).name)()", callbacks)
```

Callbacks must return `{:ok, value}` or `{:error, reason}`:

- `{:ok, value}` — value is converted to JS and returned to the caller
- `{:error, reason}` — throws a catchable JS `Error` carrying `reason`
  **verbatim** — never put a secret in it
- a **raised** exception is different: the guest sees only a generic
  `"host function failed"` exception, and if it goes uncaught the eval
  returns `{:error, %ExSafejs.Error{kind: :host_error}}` carrying the real
  exception message to the Elixir caller only

```elixir
callbacks = %{
  "risky" => fn [n] ->
    if n > 0, do: {:ok, n * 2}, else: {:error, "must be positive"}
  end
}

# JS can catch callback errors
{:ok, "must be positive"} = ExSafejs.eval(rt, """
  try { risky(-1); } catch(e) { e.message; }
""", callbacks)
```

### Lifecycle

```elixir
{:ok, rt} = ExSafejs.start()

ExSafejs.alive?(rt)  # true

# Global state persists across evals
{:ok, 42} = ExSafejs.eval(rt, "globalThis.x = 42")
{:ok, 42} = ExSafejs.eval(rt, "x")

# Stop is idempotent
:ok = ExSafejs.stop(rt)
:ok = ExSafejs.stop(rt)

ExSafejs.alive?(rt)  # false

# Eval on stopped runtime returns error (doesn't raise)
{:error, %ExSafejs.Error{kind: :dead_runtime}} = ExSafejs.eval(rt, "1")
```

## API

| Function | Description |
|----------|-------------|
| `ExSafejs.start(opts)` | Start a new JS runtime on a dedicated OS thread |
| `ExSafejs.eval(runtime, code)` | Evaluate JS code, return the result |
| `ExSafejs.eval(runtime, code, callbacks)` | Evaluate with pre-registered Elixir callbacks |
| `ExSafejs.alive?(runtime)` | Check if a runtime is alive |
| `ExSafejs.stop(runtime)` | Stop a runtime (idempotent) |

### Start Options

| Option | Type | Default | Description |
|--------|------|---------|-------------|
| `:timeout` | integer (ms) | `30_000` | Max JS compute time per eval (host-callback time excluded) |
| `:memory_limit` | integer (bytes) | `268_435_456` (256 MB) | Max JS heap allocation |
| `:max_stack_size` | integer (bytes) | `1_048_576` (1 MB) | Max JS call stack size |
| `:gc_threshold` | integer (bytes) | `4_194_304` (4 MB) | GC trigger threshold |

### Errors

Failures are `{:error, %ExSafejs.Error{kind, message, stack}}` where `kind`
is one of `:timeout`, `:deadlock`, `:memory_limit`, `:stack_overflow`,
`:js_error`, `:host_error`, `:dead_runtime`, `:start_failed` — see the
`ExSafejs.Error` moduledoc. `stack` carries the JS stack trace when the
engine provided one.

### No reserved globals

Callback dispatch is captured host-side, so ex_safejs installs nothing on
`globalThis` beyond the callback names you register (and removes those after
each eval). Guest code may shadow or delete a callback's global binding, but
that only loses its own access — nothing the guest writes is on the dispatch
path.

## Type Conversion

### JS to Elixir

| JavaScript | Elixir |
|------------|--------|
| `null`, `undefined` | `nil` |
| `true`, `false` | `true`, `false` |
| integer | integer |
| BigInt | integer (exact, arbitrary precision) |
| float | float (integer if no fractional part) |
| string | binary string |
| Array | list |
| Object | map (string keys) |
| function | `nil` |
| `NaN`, `Infinity` | `nil` |

### Elixir to JS (callback results)

| Elixir | JavaScript |
|--------|------------|
| `nil` | `null` |
| `true`, `false` | `true`, `false` |
| integer | number (up to ±2^53); BigInt beyond (exact) |
| float | number |
| binary string | string |
| atom | string |
| list | Array |
| map | Object |

## License

MIT. ExSafejs began as a fork of [quicksand](https://github.com/lpgauth/quicksand),
copyright (c) 2026 Louis-Philippe Gauthier, also MIT — see LICENSE for both notices.
