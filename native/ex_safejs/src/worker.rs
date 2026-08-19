use rquickjs::{CatchResultExt, Context, Function, Runtime, Value};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::atoms;
use crate::convert::{intermediate_to_js, js_to_term, CallbackResult, JsResult, JsValue};
use crate::runtime::{send_to_pid, CallbackRegistry};
use rustler::LocalPid;

type ResultSender = std::sync::mpsc::Sender<Result<JsValue, String>>;

pub enum Message {
    Eval(String, ResultSender),
    Stop(std::sync::mpsc::Sender<()>),
    EvalWithCallbacks(
        String,
        Vec<String>,
        Arc<CallbackRegistry>,
        LocalPid,
        Duration,
    ),
}

#[derive(Clone)]
pub struct WorkerOpts {
    pub memory_limit: usize,
    pub max_stack_size: usize,
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
        rt.set_gc_threshold(4 * 1024 * 1024);
        let interrupt_handler = Arc::clone(&interrupt);
        rt.set_interrupt_handler(Some(Box::new(move || {
            interrupt_handler.load(Ordering::Relaxed)
        })));

        let ctx =
            Context::full(&rt).map_err(|e| format!("Failed to create QuickJS context: {e}"))?;

        // Install persistent wrapper factory for callbacks.
        // Defined as non-writable, non-configurable, non-enumerable so
        // user JS cannot overwrite or delete it.
        ctx.with(|ctx| {
            let factory_code = r#"Object.defineProperty(globalThis, '__ex_safejs_make_wrapper', {
                value: function(name) {
                    return function() {
                        globalThis.__ex_safejs_cb_args = Array.from(arguments);
                        globalThis.__ex_safejs_dispatch(name);
                        var r = globalThis.__ex_safejs_cb_result;
                        delete globalThis.__ex_safejs_cb_result;
                        return r;
                    };
                },
                writable: false,
                configurable: false,
                enumerable: false
            })"#;
            ctx.eval::<(), _>(factory_code)
                .catch(&ctx)
                .map_err(|e| format!("Failed to install wrapper factory: {e:?}"))
        })?;

        Ok(Self { rt, ctx, interrupt })
    }

    pub fn run(&mut self, receiver: std::sync::mpsc::Receiver<Message>) {
        while let Ok(msg) = receiver.recv() {
            match msg {
                Message::Eval(code, tx) => {
                    self.interrupt.store(false, Ordering::Relaxed);
                    let result = self.eval(&code);
                    let _ = tx.send(result);
                }
                Message::EvalWithCallbacks(code, fn_names, callbacks, caller_pid, timeout) => {
                    self.interrupt.store(false, Ordering::Relaxed);
                    let result = match self.install_callback_functions(
                        &fn_names,
                        &callbacks,
                        &caller_pid,
                        timeout,
                    ) {
                        Ok(()) => self.eval(&code),
                        Err(e) => Err(e),
                    };
                    self.remove_callback_functions(&fn_names);
                    callbacks.clear();
                    send_to_pid(
                        &caller_pid,
                        (atoms::ex_safejs_result(), JsResult::from(result)),
                    );
                }
                Message::Stop(tx) => {
                    let _ = tx.send(());
                    break;
                }
            }
        }
    }

    fn drain_jobs(&self) {
        loop {
            match self.rt.execute_pending_job() {
                Ok(false) => break,
                Ok(true) => continue,
                Err(_) => break,
            }
        }
    }

    fn eval(&mut self, code: &str) -> Result<JsValue, String> {
        let result = self
            .ctx
            .with(|ctx| match ctx.eval::<Value, _>(code).catch(&ctx) {
                Ok(val) => Ok(js_to_term(&ctx, val)),
                Err(e) => Ok(Err(format_caught_error(e))),
            });

        match result {
            Ok(r) => {
                self.drain_jobs();
                r
            }
            Err(e) => Err(e),
        }
    }

    fn install_callback_functions(
        &self,
        fn_names: &[String],
        callbacks: &Arc<CallbackRegistry>,
        caller_pid: &LocalPid,
        timeout: Duration,
    ) -> Result<(), String> {
        let cb = Arc::clone(callbacks);
        let pid = *caller_pid;

        self.ctx.with(|ctx| {
            let globals = ctx.globals();

            // __ex_safejs_dispatch(name) → string sentinel or throw
            // JS wrapper stores args in __ex_safejs_cb_args global before calling.
            // Dispatch reads args directly as JS Values via js_to_term (no JSON).
            // Result is stored in __ex_safejs_cb_result global (avoids Value lifetime issues).
            let dispatch = Function::new(
                ctx.clone(),
                move |ctx: rquickjs::Ctx<'_>, name: String| -> rquickjs::Result<String> {
                    let args_val: rquickjs::Value = ctx
                        .globals()
                        .get("__ex_safejs_cb_args")
                        .unwrap_or_else(|_| rquickjs::Value::new_undefined(ctx.clone()));

                    let callback_args = match js_to_term(&ctx, args_val) {
                        Ok(JsValue::Array(items)) => items,
                        Ok(other) => vec![other],
                        Err(_) => vec![],
                    };

                    let (id, rx) = cb.register();

                    send_to_pid(
                        &pid,
                        (atoms::ex_safejs_callback(), id, name.clone(), callback_args),
                    );

                    match rx.recv_timeout(timeout) {
                        Ok(CallbackResult::Ok(term_value)) => {
                            let js_val = intermediate_to_js(&ctx, &term_value)
                                .map_err(|_| rquickjs::Error::Exception)?;
                            ctx.globals()
                                .set("__ex_safejs_cb_result", js_val)
                                .map_err(|_| rquickjs::Error::Exception)?;
                            Ok("__ok__".to_string())
                        }
                        Ok(CallbackResult::Err(reason)) => {
                            ctx.throw(
                                rquickjs::String::from_str(ctx.clone(), &reason)
                                    .map_err(|_| rquickjs::Error::Exception)?
                                    .into(),
                            );
                            Err(rquickjs::Error::Exception)
                        }
                        Err(_) => {
                            ctx.throw(
                                rquickjs::String::from_str(
                                    ctx.clone(),
                                    &format!("Callback '{}' timed out", name),
                                )
                                .map_err(|_| rquickjs::Error::Exception)?
                                .into(),
                            );
                            Err(rquickjs::Error::Exception)
                        }
                    }
                },
            )
            .map_err(|e| format!("Failed to create __ex_safejs_dispatch: {e}"))?;

            globals
                .set("__ex_safejs_dispatch", dispatch)
                .map_err(|e| format!("Failed to set __ex_safejs_dispatch: {e}"))?;

            // Use persistent factory installed in Worker::new.
            // Callback name is passed as a parameter, never interpolated
            // into code, preventing JS injection.
            let make_wrapper: Function = globals
                .get("__ex_safejs_make_wrapper")
                .map_err(|e| format!("Failed to get wrapper factory: {e}"))?;

            for fn_name in fn_names {
                let wrapper: Function = make_wrapper
                    .call((fn_name.as_str(),))
                    .catch(&ctx)
                    .map_err(|e| format!("Failed to create wrapper for {fn_name}: {e:?}"))?;
                globals
                    .set(fn_name.as_str(), wrapper)
                    .map_err(|e| format!("Failed to set wrapper for {fn_name}: {e}"))?;
            }

            Ok(())
        })
    }

    fn remove_callback_functions(&self, fn_names: &[String]) {
        self.ctx.with(|ctx| {
            let globals = ctx.globals();
            let _ = globals.remove("__ex_safejs_dispatch");
            let _ = globals.remove("__ex_safejs_cb_args");
            let _ = globals.remove("__ex_safejs_cb_result");
            for fn_name in fn_names {
                let _ = globals.remove(fn_name.as_str());
            }
        });
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
