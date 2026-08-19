use rquickjs::function::Rest;
use rquickjs::{CatchResultExt, Context, Function, Persistent, Promise, Runtime, Value};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::atoms;
use crate::convert::{intermediate_to_js, js_to_term, CallbackResult, EvalResult, JsValue};
use crate::runtime::{send_to_pid, CallbackRegistry};
use rustler::LocalPid;

/// Upper bound on jobs executed per drain pass. The drive loop re-enters, so
/// this yields (interrupt checks between passes) rather than truncates; it
/// exists so an adversarial promise storm can't starve the interrupt check.
const MAX_JOBS_PER_PASS: usize = 100_000;

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
    Done(Result<JsValue, String>),
    Pending(Persistent<Promise<'static>>),
}

pub struct Worker {
    rt: Runtime,
    ctx: Context,
    interrupt: Arc<AtomicBool>,
}

impl Worker {
    pub fn new(opts: WorkerOpts, interrupt: Arc<AtomicBool>) -> Result<Self, String> {
        let rt = Runtime::new().map_err(|e| format!("Failed to create QuickJS runtime: {e}"))?;
        rt.set_memory_limit(opts.memory_limit);
        rt.set_max_stack_size(opts.max_stack_size);
        rt.set_gc_threshold(opts.gc_threshold);
        let interrupt_handler = Arc::clone(&interrupt);
        rt.set_interrupt_handler(Some(Box::new(move || {
            interrupt_handler.load(Ordering::Relaxed)
        })));

        let ctx =
            Context::full(&rt).map_err(|e| format!("Failed to create QuickJS context: {e}"))?;

        Ok(Self { rt, ctx, interrupt })
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
                    self.interrupt.store(false, Ordering::Relaxed);
                    let result =
                        match self.install_callbacks(&fn_names, &callbacks, &caller, eval_id) {
                            Ok(()) => self.eval(&code),
                            Err(e) => Err(EvalError::js(e)),
                        };
                    self.remove_callbacks(&fn_names);
                    callbacks.clear();
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
    }

    fn interrupted(&self) -> bool {
        self.interrupt.load(Ordering::Relaxed)
    }

    /// Execute up to MAX_JOBS_PER_PASS pending jobs. A job that raises is
    /// still consumed — its error surfaces through the promise graph, not
    /// here — but an interrupt ends the pass immediately.
    fn drain_pass(&self) -> usize {
        let mut executed = 0;
        while executed < MAX_JOBS_PER_PASS {
            match self.rt.execute_pending_job() {
                Ok(true) => executed += 1,
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

    fn eval(&self, code: &str) -> Result<JsValue, EvalError> {
        let outcome = self.ctx.with(|ctx| {
            match ctx.eval::<Value, _>(code).catch(&ctx) {
                Err(e) => EvalOutcome::Done(Err(format_caught_error(e))),
                Ok(v) => {
                    if let Some(p) = v.as_promise() {
                        // Async-aware path: an async arrow (or any code whose
                        // completion value is a promise) is driven to
                        // settlement below instead of snapshotting as `{}`.
                        EvalOutcome::Pending(Persistent::save(&ctx, p.clone()))
                    } else {
                        EvalOutcome::Done(js_to_term(&ctx, v))
                    }
                }
            }
        });

        match outcome {
            EvalOutcome::Done(r) => {
                // Sync completion value is already snapshotted; still drain
                // side-effect jobs the guest scheduled. Exceeding the budget
                // inside one of them is the guest's fault.
                loop {
                    let executed = self.drain_pass();
                    if executed == 0 || self.interrupted() {
                        break;
                    }
                }
                if self.interrupted() {
                    return Err(EvalError::timeout());
                }
                r.map_err(EvalError::js)
            }
            EvalOutcome::Pending(p) => self.drive(p),
        }
    }

    /// Drain → check state → settle, in that order (a pure `.then` chain must
    /// settle inside the drain). Pending with an empty job queue is a
    /// provable deadlock: host calls are synchronous, so none can be in
    /// flight while we are here.
    fn drive(&self, saved: Persistent<Promise<'static>>) -> Result<JsValue, EvalError> {
        loop {
            let executed = self.drain_pass();
            if self.interrupted() {
                return Err(EvalError::timeout());
            }

            let settled = self.ctx.with(|ctx| {
                let p = match saved.clone().restore(&ctx) {
                    Ok(p) => p,
                    Err(e) => return Some(Err(format!("promise restore failed: {e}"))),
                };
                p.result::<Value>().map(|r| match r.catch(&ctx) {
                    Ok(v) => js_to_term(&ctx, v),
                    Err(e) => Err(format_caught_error(e)),
                })
            });

            match settled {
                Some(r) => return r.map_err(EvalError::js),
                None if executed == 0 => return Err(EvalError::deadlock()),
                None => continue,
            }
        }
    }

    /// Install each Elixir callback as a real JS `Function` whose dispatch
    /// key (the callback name) is captured in the Rust closure. Nothing the
    /// guest can read or write is on the dispatch path — it may shadow or
    /// delete the global binding, but that only loses its own access. This
    /// replaces the old reserved-global (`__*_dispatch`/`__*_cb_args`)
    /// machinery entirely.
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
                let dispatch =
                    make_dispatch(Arc::clone(callbacks), *caller, fn_name.clone(), eval_id);

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
    pid: LocalPid,
    name: String,
    eval_id: u64,
) -> impl for<'js> Fn(rquickjs::Ctx<'js>, Rest<Value<'js>>) -> rquickjs::Result<Value<'js>> {
    move |fctx, args| {
        let mut term_args = Vec::with_capacity(args.0.len());
        for a in args.0 {
            term_args.push(js_to_term(&fctx, a).unwrap_or(JsValue::Null));
        }

        let (cb_id, rx) = cb.register();
        send_to_pid(
            &pid,
            (
                atoms::ex_safejs_callback(),
                eval_id,
                cb_id,
                name.clone(),
                term_args,
            ),
        );

        // Block until the host responds. There is deliberately no timeout
        // here: the Elixir side owns all timing (host-call wall time does not
        // count against the JS deadline), and stop/Drop close the registry,
        // which disconnects this channel and unblocks us.
        match rx.recv() {
            Ok(CallbackResult::Ok(tv)) => intermediate_to_js(&fctx, &tv).map_err(|e| {
                rquickjs::Exception::throw_message(
                    &fctx,
                    &format!("failed to convert callback result: {e}"),
                )
            }),
            Ok(CallbackResult::Err(reason)) => {
                Err(rquickjs::Exception::throw_message(&fctx, &reason))
            }
            Err(_) => Err(rquickjs::Exception::throw_message(
                &fctx,
                "callback aborted: runtime is stopping",
            )),
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
                format!("{val:?}")
            }
        }
        rquickjs::CaughtError::Value(val) => format!("Thrown value: {val:?}"),
        rquickjs::CaughtError::Error(e) => format!("{e}"),
    }
}
