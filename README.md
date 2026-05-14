# Quicksand

Sandboxed JavaScript execution for Elixir via [QuickJS-NG](https://github.com/quickjs-ng/quickjs).

Quicksand embeds the QuickJS-NG engine as a Rustler NIF, giving you in-process JS evaluation with strict memory and time limits. Each runtime runs on a dedicated OS thread — JS execution never blocks BEAM schedulers.

## Features

- Sandboxed JS with no filesystem, network, or OS access
- Configurable memory limit, execution timeout, and stack size
- Pre-registered Elixir callbacks callable from JS
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
    {:quicksand, "~> 0.1.0"}
  ]
end
```

Precompiled binaries will be downloaded automatically. To build from source instead:

```bash
QUICKSAND_BUILD=true mix deps.compile quicksand
```

## Usage

### Basic Evaluation

```elixir
{:ok, rt} = Quicksand.start()

{:ok, 3} = Quicksand.eval(rt, "1 + 2")
{:ok, "hello"} = Quicksand.eval(rt, "'hello'")
{:ok, %{"a" => 1}} = Quicksand.eval(rt, "({a: 1})")

:ok = Quicksand.stop(rt)
```

### Resource Limits

```elixir
{:ok, rt} = Quicksand.start(
  timeout: 5_000,            # 5 seconds max execution time
  memory_limit: 10_000_000,  # ~10 MB heap limit
  max_stack_size: 512_000    # 512 KB stack
)

# Infinite loops are interrupted
{:error, "timeout"} = Quicksand.eval(rt, "while(true) {}")

# Runtime remains usable after timeout
{:ok, 42} = Quicksand.eval(rt, "42")
```

### Callbacks

Register Elixir functions that JS code can call synchronously:

```elixir
{:ok, rt} = Quicksand.start()

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

{:ok, "Alice"} = Quicksand.eval(rt, """
  const user = fetch_user(1);
  log("Found user: " + user.name);
  user.name;
""", callbacks)
```

Callbacks must return `{:ok, value}` or `{:error, reason}`:

- `{:ok, value}` — value is converted to JS and returned to the caller
- `{:error, reason}` — throws a JS exception with the reason as the message

```elixir
callbacks = %{
  "risky" => fn [n] ->
    if n > 0, do: {:ok, n * 2}, else: {:error, "must be positive"}
  end
}

# JS can catch callback errors
{:ok, "must be positive"} = Quicksand.eval(rt, """
  try { risky(-1); } catch(e) { e.message; }
""", callbacks)
```

### Lifecycle

```elixir
{:ok, rt} = Quicksand.start()

Quicksand.alive?(rt)  # true

# Global state persists across evals
{:ok, 42} = Quicksand.eval(rt, "globalThis.x = 42")
{:ok, 42} = Quicksand.eval(rt, "x")

# Stop is idempotent
:ok = Quicksand.stop(rt)
:ok = Quicksand.stop(rt)

Quicksand.alive?(rt)  # false

# Eval on stopped runtime returns error (doesn't raise)
{:error, "dead_runtime"} = Quicksand.eval(rt, "1")
```

## API

| Function | Description |
|----------|-------------|
| `Quicksand.start(opts)` | Start a new JS runtime on a dedicated OS thread |
| `Quicksand.eval(runtime, code)` | Evaluate JS code, return the result |
| `Quicksand.eval(runtime, code, callbacks)` | Evaluate with pre-registered Elixir callbacks |
| `Quicksand.alive?(runtime)` | Check if a runtime is alive |
| `Quicksand.stop(runtime)` | Stop a runtime (idempotent) |

### Start Options

| Option | Type | Default | Description |
|--------|------|---------|-------------|
| `:timeout` | integer (ms) | `30_000` | Max JS execution time per eval |
| `:memory_limit` | integer (bytes) | `268_435_456` (256 MB) | Max JS heap allocation |
| `:max_stack_size` | integer (bytes) | `1_048_576` (1 MB) | Max JS call stack size |

### Reserved global names

When `eval/3` is in callback mode, quicksand installs four `globalThis.__quicksand_*` properties on the JS runtime to plumb callback dispatch:

- `__quicksand_make_wrapper` — factory that builds the Elixir-callback wrapper functions injected as JS globals.
- `__quicksand_dispatch` — called by the wrapper to message the Elixir process.
- `__quicksand_cb_args` — slot the wrapper writes per-call arguments into.
- `__quicksand_cb_result` — slot Elixir writes the result into.

Don't define these in your user JS — overwriting them silently breaks all Elixir callbacks in that runtime. The leading double-underscore is a convention signaling "implementation detail, don't touch", matching the same convention QuickJS-NG uses internally.

## Type Conversion

### JS to Elixir

| JavaScript | Elixir |
|------------|--------|
| `null`, `undefined` | `nil` |
| `true`, `false` | `true`, `false` |
| integer | integer |
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
| integer | number |
| float | number |
| binary string | string |
| atom | string |
| list | Array |
| map | Object |

## License

MIT
