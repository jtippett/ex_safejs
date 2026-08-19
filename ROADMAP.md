# Renovation roadmap

ex_safejs v0.2.0 is quicksand 0.1.1 + the rquickjs 0.12 bump, API-unchanged.
Now that the API is ours to break, this is the renovation plan. Much of it is
informed by a source read of **langchain-ai/quickjs-rs** (Python, QuickJS-NG
in WASM) — the same architecture, independently security-assessed, whose
threat model and async design are worth stealing from and whose mistakes are
worth naming. Citations below refer to that repo.

## A. Async-aware eval (the headline)

The current `eval` snapshots the result **before** draining pending jobs, so
a returned promise is an unresolved `{}` and guest code must be synchronous.
Fix, following quickjs-rs's shape:

- Keep sync `eval/2`; add `eval_async` built on `Ctx::eval_promise`
  (`JS_EVAL_FLAG_ASYNC`) — top-level `await` and last-expression-value
  semantics for free. rquickjs 0.12 exposes this; it's the reason the
  interrupt-during-job abort fix mattered.
- Drive loop in the exact order **drain → check promise state → settle**
  (a pure `.then` chain must settle inside the drain, no external event).
- Bounded drain (~100k jobs per pass, outer loop re-enters) so a promise
  storm yields instead of wedging.
- **Deadlock detection**: promise pending + job queue empty + zero in-flight
  host calls = provably stuck → structured `:deadlock` error naming the
  usual causes (`new Promise(() => {})`, forgotten resolve, sync-registered
  callback that should be async). For LLM-written JS this is *the* common
  failure and today it burns the whole timeout silently.
- **Exclude host-callback wall time from the JS deadline** (their best
  single idea): pause the deadline while a host call runs, so "timeout"
  means JS compute budget, not wall clock — a slow API call must not read
  as guest misbehavior.
- After an abort/timeout decision, run **one final drain** so guest
  `catch`/`finally` cleanup executes before teardown.
- Wire `JS_SetHostPromiseRejectionTracker` (they didn't — silently
  swallowed rejections are a debugging nightmare with LLM code).

## B. Callback bridge redesign

- **Kill the reserved globals** (`__ex_safejs_make_wrapper/_dispatch/
  _cb_args/_cb_result`). Mint each host function as a real JS `Function`
  whose Rust closure captures the callback name; nothing the guest can
  write is on the dispatch path (quickjs-rs `hostfn.rs`: guest may shadow
  or delete `globalThis.myFn` and it changes nothing).
- Unified trampoline: sync-vs-async is a host decision — the callback
  returns a value or `{:promise, ref}`; a later `settle` call resolves or
  rejects the guest-side deferred, then drains. Remove-on-settle makes
  double-settle structurally impossible; use collision-safe ids (their
  Gen-2 shipped a `wrapping_add` id that can settle the *wrong* promise —
  adopt their Gen-1 fix, not the regression).
- **Ref-tagged messages**: caller supplies a ref, every
  `{:ex_safejs_result, ...}`/`{:ex_safejs_callback, ...}` carries it, so a
  timed-out eval's straggler can't poison an unrelated `receive`. (This is
  why oapi_codemode's executor currently burns a throwaway process per
  eval.) Cleanup of pending deferreds must run on **every** abnormal exit —
  they plugged cancellation and left the timeout path leaking.
- **Sanitized host errors**: guest sees a fixed `HostError: host function
  failed` with no detail; the Elixir caller gets the real exception via a
  side channel cleared at each eval entry. Untrusted JS must not be able to
  read host failure detail out of a `catch`.

## C. Errors and conversion

- Structured errors: `{:error, %ExSafejs.Error{kind, message, stack}}` with
  `kind` in `:timeout | :memory_limit | :stack_overflow | :deadlock |
  :js_error | :host_error | :dead_runtime`. Classification is string-matching
  on QuickJS-NG internals ("interrupted", "out of memory") — pin those
  strings with tests since they're an upstream contract that can drift.
  An LLM repair loop can branch on `kind`; today it regexes a message.
- Handle non-`Error` throws (`throw 42`, `throw 'x'`, `throw null`): coerce
  via `String(v)`, and gate on `has_exception()` before catching so a thrown
  `null` isn't confused with "no exception".
- **BigInt round-trip**: Elixir integers are arbitrary-precision; anything
  beyond ±2^53 must cross as BigInt via decimal string, both directions —
  never a silent f64 truncation. (Audit convert.rs for this today.)
- Depth cap (~128) on both conversion directions with a "cycle or deeply
  nested" error — a self-referential return value is a one-line DoS against
  a naive recursive converter.
- Promise special-case in JS→term conversion: a promise is an object with no
  own enumerable keys, so a naive walk silently returns `%{}` — either
  convert its settled value or return a typed error, never an empty map.

## D. Hardening odds and ends

- Decide a `Date.now`/`Math.random` policy (quickjs-rs made no decision —
  their weakest area). Likely: opts to freeze/seed for reproducibility,
  default off.
- Defend the working stack limit: deep recursion must stay a **catchable
  error with a surviving runtime** — this is a genuine advantage over the
  WASM build, where quickjs-ng compiles the stack check out and recursion
  hard-traps the instance. Add a pinning test.
- Interrupt honesty: if the interrupt fires, verify the deadline actually
  elapsed before reporting `:timeout` (vs a generic interrupt); if the
  interrupt handler itself fails, fail closed (stop the guest).
- Regression tests to adopt: timeout state resets between evals; interleaved
  multi-runtime isolation with per-runtime secret tokens re-verified every
  turn (catches message-routing bugs in the channel/thread model).
- Make `gc_threshold` configurable (currently hard-coded 4 MiB; interacts
  badly with small memory limits — recovery-at-cap behavior is erratic).

## Explicitly rejected

- **WASM rewrite** (their Gen-2): costs a working stack limit, adds an
  uncapped second allocator and a slow interrupt path. Our claim discipline
  (README: resource guards, not host-memory isolation) plus BEAM-level
  process isolation for hostile multi-tenant work covers the same threat
  more cheaply.
- **Fuel/epoch metering**: interrupt-counter + memory limit already cover
  our surface; no second allocator to escape through.
- **Module loading**: no `import()` support exists and none is planned; if
  it ever is, adopt their host-owned policy shape (resolver/loader
  callbacks, `nil` = refuse, caps on specifier and source length,
  memoized resolution — and note the callbacks must answer synchronously).
