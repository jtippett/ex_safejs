use rquickjs::function::Rest;
use rquickjs::{CatchResultExt, Context, Function, Persistent, Promise, Runtime, Value};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::atoms;
use crate::convert::{intermediate_to_js, js_to_term, CallbackResult, ConvertBudget, EvalResult};
use crate::runtime::{send_to_pid, CallbackRegistry, EvalControl};
use rustler::LocalPid;

/// Upper bound on jobs executed per drain pass. The drive loop re-enters, so
/// this yields (interrupt checks between passes) rather than truncates; it
/// exists so an adversarial promise storm can't starve the interrupt check.
const MAX_JOBS_PER_PASS: usize = 100_000;

/// Poll interval for a dispatch blocked on a callback response. A response
/// wakes the wait immediately; this only bounds how long a blocked dispatch
/// waits before re-checking whether its eval was cancelled or the runtime is
/// stopping, so an abandoned callback (dead caller, absorbed timeout) can't
/// wedge the worker forever.
const DISPATCH_POLL: Duration = Duration::from_millis(50);

pub enum Message {
    Eval {
        code: String,
        fn_names: Vec<String>,
        callbacks: Arc<CallbackRegistry>,
        caller: LocalPid,
        eval_id: u64,
    },
    Stop(std::sync::mpsc::Sender<()>),
}

#[derive(Clone)]
pub struct WorkerOpts {
    pub memory_limit: usize,
    pub max_stack_size: usize,
    pub gc_threshold: usize,
}

pub struct EvalError {
    pub kind: rustler::Atom,
    pub message: String,
}

impl EvalError {
    fn timeout() -> Self {
        Self {
            kind: atoms::timeout(),
            message: "interrupted".to_string(),
        }
    }

    fn deadlock() -> Self {
        Self {
            kind: atoms::deadlock(),
            message: "the top-level promise is still pending but the job queue is empty \
                      and no host call is in flight, so nothing can ever settle it \
                      (e.g. `new Promise(() => {})` or a forgotten resolve())"
                .to_string(),
        }
    }

    fn js(message: String) -> Self {
        Self {
            kind: atoms::js_error(),
            message,
        }
    }
}

enum EvalOutcome {
    Value(Result<crate::convert::JsValue, String>),
    Pending(Persistent<Promise<'static>>),
}

pub struct Worker {
    rt: Runtime,
    ctx: Context,
    control: Arc<EvalControl>,
    alive: Arc<AtomicBool>,
}

impl Worker {
    pub fn new(
        opts: WorkerOpts,
        control: Arc<EvalControl>,
        alive: Arc<AtomicBool>,
    ) -> Result<Self, String> {
        let rt = Runtime::new().map_err(|e| format!("Failed to create QuickJS runtime: {e}"))?;
        rt.set_memory_limit(opts.memory_limit);
        rt.set_max_stack_size(opts.max_stack_size);
        rt.set_gc_threshold(opts.gc_threshold);
        // The interrupt handler is eval-scoped: it fires only for the eval
        // currently running (or when the runtime is stopping), so one eval's
        // deadline can never abort another's.
        let handler_control = Arc::clone(&control);
        rt.set_interrupt_handler(Some(Box::new(move || handler_control.should_interrupt())));

        let ctx =
            Context::full(&rt).map_err(|e| format!("Failed to create QuickJS context: {e}"))?;

        Ok(Self {
            rt,
            ctx,
            control,
            alive,
        })
    }

    pub fn run(&mut self, receiver: std::sync::mpsc::Receiver<Message>) {
        while let Ok(msg) = receiver.recv() {
            match msg {
                Message::Eval {
                    code,
                    fn_names,
                    callbacks,
                    caller,
                    eval_id,
                } => {
                    self.control.enter(eval_id);

                    // If this eval's deadline already fired while it was
                    // queued (or the runtime is stopping), don't run any guest
                    // code — report the timeout and move on. Running it would
                    // burn a core uninterruptibly, since the interrupt that
                    // was meant for it has already been observed.
                    let result = if self.control.is_cancelled(eval_id) {
                        Err(EvalError::timeout())
                    } else {
                        match self.install_callbacks(&fn_names, &callbacks, &caller, eval_id) {
                            Ok(()) => self.eval(&code),
                            Err(e) => Err(EvalError::js(e)),
                        }
                    };

                    self.remove_callbacks(&fn_names);
                    callbacks.clear();
                    self.control.leave();
                    send_to_pid(
                        &caller,
                        (atoms::ex_safejs_result(), eval_id, EvalResult::from(result)),
                    );
                }
                Message::Stop(tx) => {
                    let _ = tx.send(());
                    break;
                }
            }
        }
        // The worker loop has ended (Stop, or the sender dropped): the runtime
        // is unusable, so reflect that in `alive?`.
        self.alive.store(false, Ordering::Relaxed);
    }

    fn interrupted(&self) -> bool {
        self.control.should_interrupt()
    }

    /// Execute up to MAX_JOBS_PER_PASS pending jobs. A job that raises is
    /// still consumed — its error surfaces through the promise graph, not
    /// here — but an interrupt ends the pass immediately.
    fn drain_pass(&self) -> usize {
        let mut executed = 0;
        while executed < MAX_JOBS_PER_PASS {
            match self.rt.execute_pending_job() {
                Ok(true) => {
                    executed += 1;
                    if executed % 4096 == 0 && self.interrupted() {
                        break;
                    }
                }
                Ok(false) => break,
                Err(_job_error) => {
                    executed += 1;
                    if self.interrupted() {
                        break;
                    }
                }
            }
        }
        executed
    }

    fn eval(&self, code: &str) -> Result<crate::convert::JsValue, EvalError> {
        let outcome = self.ctx.with(|ctx| {
            match ctx.eval::<Value, _>(code).catch(&ctx) {
                Err(e) => EvalOutcome::Value(Err(format_caught_error(e))),
                Ok(v) => {
                    if let Some(p) = v.as_promise() {
                        // Async-aware path: an async arrow (or any code whose
                        // completion value is a promise) is driven to
                        // settlement below instead of snapshotting as `{}`.
                        EvalOutcome::Pending(Persistent::save(&ctx, p.clone()))
                    } else {
                        let mut budget = ConvertBudget::new(Some(&self.control));
                        EvalOutcome::Value(js_to_term(&ctx, v, &mut budget))
                    }
                }
            }
        });

        match outcome {
            EvalOutcome::Value(r) => {
                self.finish_sync_jobs();
                if self.interrupted() {
                    return Err(EvalError::timeout());
                }
                r.map_err(EvalError::js)
            }
            EvalOutcome::Pending(p) => self.drive(p),
        }
    }

    /// Drain side-effect jobs the guest scheduled after a synchronous
    /// completion value was already snapshotted.
    fn finish_sync_jobs(&self) {
        loop {
            let executed = self.drain_pass();
            if executed == 0 || self.interrupted() {
                break;
            }
        }
    }

    /// Drive → check state → settle, in that order (a pure `.then` chain must
    /// settle inside the drain). Pending with an empty job queue is a
    /// provable deadlock: host calls are synchronous, so none can be in
    /// flight while we are here.
    fn drive(
        &self,
        saved: Persistent<Promise<'static>>,
    ) -> Result<crate::convert::JsValue, EvalError> {
        let result = loop {
            let executed = self.drain_pass();
            if self.interrupted() {
                break Err(EvalError::timeout());
            }

            let settled = self.ctx.with(|ctx| {
                let p = match saved.clone().restore(&ctx) {
                    Ok(p) => p,
                    Err(e) => return Some(Err(format!("promise restore failed: {e}"))),
                };
                p.result::<Value>().map(|r| match r.catch(&ctx) {
                    Ok(v) => {
                        let mut budget = ConvertBudget::new(Some(&self.control));
                        js_to_term(&ctx, v, &mut budget)
                    }
                    Err(e) => Err(format_caught_error(e)),
                })
            });

            match settled {
                Some(r) => break r.map_err(EvalError::js),
                None if executed == 0 => break Err(EvalError::deadlock()),
                None => continue,
            }
        };

        // Free the retained promise handle under the runtime lock; dropping a
        // Persistent<Value> outside `ctx.with` would run JS_FreeValue without
        // it.
        self.ctx.with(move |ctx| {
            let _ = saved.restore(&ctx);
        });

        result
    }

    /// Install each Elixir callback as a real JS `Function` whose dispatch
    /// key (the callback name) is captured in the Rust closure. Nothing the
    /// guest can read or write is on the dispatch path — it may shadow or
    /// delete the global binding, but that only loses its own access, and a
    /// reference retained past its eval is refused by the generation check in
    /// `make_dispatch`. This replaces the old reserved-global machinery.
    fn install_callbacks(
        &self,
        fn_names: &[String],
        callbacks: &Arc<CallbackRegistry>,
        caller: &LocalPid,
        eval_id: u64,
    ) -> Result<(), String> {
        self.ctx.with(|ctx| {
            let globals = ctx.globals();

            for fn_name in fn_names {
                let dispatch = make_dispatch(
                    Arc::clone(callbacks),
                    Arc::clone(&self.control),
                    *caller,
                    fn_name.clone(),
                    eval_id,
                );

                let f = Function::new(ctx.clone(), dispatch)
                    .map_err(|e| format!("Failed to create callback function {fn_name}: {e}"))?;

                globals
                    .set(fn_name.as_str(), f)
                    .map_err(|e| format!("Failed to set callback {fn_name}: {e}"))?;
            }

            Ok(())
        })
    }

    fn remove_callbacks(&self, fn_names: &[String]) {
        self.ctx.with(|ctx| {
            let globals = ctx.globals();
            for fn_name in fn_names {
                let _ = globals.remove(fn_name.as_str());
            }
        });
    }
}

/// Build the dispatch closure for one Elixir callback. Returned as an
/// `impl for<'js> Fn` so the closure is higher-ranked over the JS lifetime —
/// it must return a `Value<'js>` tied to the invocation context, which an
/// inline closure's inferred (scope-pinned) lifetimes can't express.
fn make_dispatch(
    cb: Arc<CallbackRegistry>,
    control: Arc<EvalControl>,
    pid: LocalPid,
    name: String,
    eval_id: u64,
) -> impl for<'js> Fn(rquickjs::Ctx<'js>, Rest<Value<'js>>) -> rquickjs::Result<Value<'js>> {
    move |fctx, args| {
        // Generation check: a callback reference the guest stashed in a prior
        // eval and called in a later one belongs to a completed eval. Refuse
        // it before touching the registry or the caller's mailbox, so a stale
        // reference can neither wedge the worker nor inject a message into an
        // unrelated process.
        if !control.is_running(eval_id) {
            return Err(rquickjs::Exception::throw_message(
                &fctx,
                "host function is not callable outside the evaluation that installed it",
            ));
        }

        let mut term_args = Vec::with_capacity(args.0.len());
        let mut budget = ConvertBudget::new(Some(&control));
        for a in args.0 {
            match js_to_term(&fctx, a, &mut budget) {
                Ok(v) => term_args.push(v),
                Err(e) => {
                    return Err(rquickjs::Exception::throw_message(
                        &fctx,
                        &format!("could not convert callback argument: {e}"),
                    ));
                }
            }
        }

        let (cb_id, rx) = cb.register();
        if !send_to_pid(
            &pid,
            (
                atoms::ex_safejs_callback(),
                eval_id,
                cb_id,
                name.clone(),
                term_args,
            ),
        ) {
            return Err(rquickjs::Exception::throw_message(
                &fctx,
                "callback aborted: the calling process is gone",
            ));
        }

        // Block until the host responds. The Elixir side owns wall-clock
        // timing (host-call time is excluded from the JS deadline), so there
        // is no timeout on the response itself — but we wake periodically to
        // check whether this eval was cancelled or the runtime is stopping,
        // so an abandoned callback can't park the worker thread forever.
        loop {
            match rx.recv_timeout(DISPATCH_POLL) {
                Ok(CallbackResult::Ok(tv)) => {
                    return intermediate_to_js(&fctx, &tv).map_err(|e| {
                        rquickjs::Exception::throw_message(
                            &fctx,
                            &format!("failed to convert callback result: {e}"),
                        )
                    });
                }
                Ok(CallbackResult::Err(reason)) => {
                    return Err(rquickjs::Exception::throw_message(&fctx, &reason));
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(rquickjs::Exception::throw_message(
                        &fctx,
                        "callback aborted: runtime is stopping",
                    ));
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    if control.is_cancelled(eval_id) {
                        return Err(rquickjs::Exception::throw_message(
                            &fctx,
                            "callback aborted: evaluation was cancelled",
                        ));
                    }
                }
            }
        }
    }
}

fn format_caught_error(err: rquickjs::CaughtError<'_>) -> String {
    match err {
        rquickjs::CaughtError::Exception(val) => {
            if val.is_object() {
                let obj = val.as_object();
                let message: String = obj.get("message").unwrap_or_default();
                let stack: String = obj.get("stack").unwrap_or_default();
                if stack.is_empty() {
                    message
                } else {
                    format!("{message}\n{stack}")
                }
            } else {
                coerce_thrown(&val)
            }
        }
        // A non-`Error` throw (`throw 42`, `throw 'x'`, `throw {…}`) arrives
        // here. Coerce it through JS `String(v)` rather than Rust `Debug`,
        // which would emit internal wrappers and even leak a host heap
        // address for objects.
        rquickjs::CaughtError::Value(val) => coerce_thrown(&val),
        rquickjs::CaughtError::Error(e) => format!("{e}"),
    }
}

fn coerce_thrown(val: &Value<'_>) -> String {
    let ctx = val.ctx().clone();
    match rquickjs::FromJs::from_js(&ctx, val.clone()) {
        Ok(rquickjs::Coerced(s)) => s,
        Err(_) => "uncoercible thrown value".to_string(),
    }
}
