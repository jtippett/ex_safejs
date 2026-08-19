mod atoms;
mod convert;
mod runtime;
mod worker;

use convert::{term_to_intermediate, CallbackResult};
use rustler::{Env, ResourceArc, Term};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

#[rustler::nif]
#[allow(clippy::too_many_arguments)]
fn start_runtime(
    env: Env,
    start_ref: Term,
    timeout_ms: u64,
    memory_limit: usize,
    max_stack_size: usize,
    gc_threshold: usize,
) -> rustler::Atom {
    let task_pid = env.pid();
    let (sender, receiver) = std::sync::mpsc::channel::<worker::Message>();
    let control = Arc::new(runtime::EvalControl::new());
    let alive = Arc::new(AtomicBool::new(true));
    let callbacks = Arc::new(runtime::CallbackRegistry::new());
    let control_worker = Arc::clone(&control);
    let alive_worker = Arc::clone(&alive);
    let timeout = Duration::from_millis(timeout_ms);

    // Save the ref so we can tag the reply message
    let mut ref_env = rustler::OwnedEnv::new();
    let saved_ref = ref_env.save(start_ref);

    let opts = worker::WorkerOpts {
        memory_limit,
        max_stack_size,
        gc_threshold,
    };

    std::thread::spawn(
        move || match worker::Worker::new(opts, control_worker, alive_worker) {
            Ok(mut w) => {
                let sent = ref_env.send_and_clear(&task_pid, |env| {
                    let ref_term = saved_ref.load(env);
                    (
                        atoms::ex_safejs_start(),
                        ref_term,
                        (
                            atoms::ok(),
                            ResourceArc::new(runtime::Runtime::new(
                                sender, control, alive, callbacks, timeout,
                            )),
                        ),
                    )
                });
                if sent.is_ok() {
                    w.run(receiver);
                }
            }
            Err(msg) => {
                let _ = ref_env.send_and_clear(&task_pid, |env| {
                    let ref_term = saved_ref.load(env);
                    (atoms::ex_safejs_start(), ref_term, (atoms::error(), msg))
                });
            }
        },
    );

    atoms::ok()
}

/// Kick off an eval. The result arrives as `{:ex_safejs_result, eval_id,
/// result}` in the caller's mailbox; callback requests arrive as
/// `{:ex_safejs_callback, eval_id, cb_id, name, args}`. All timing (the
/// deadline, host-call time exclusion, interrupting) is owned by the Elixir
/// side.
#[rustler::nif]
fn eval_start(
    env: Env,
    resource: ResourceArc<runtime::Runtime>,
    eval_id: u64,
    code: String,
    fn_names: Vec<String>,
) -> rustler::Atom {
    let caller = env.pid();
    let callbacks = Arc::clone(&resource.callbacks);

    if resource
        .send(worker::Message::Eval {
            code,
            fn_names,
            callbacks,
            caller,
            eval_id,
        })
        .is_err()
    {
        return atoms::dead_runtime();
    }
    atoms::ok()
}

#[rustler::nif]
fn respond_callback(
    resource: ResourceArc<runtime::Runtime>,
    callback_id: u64,
    result: Term,
) -> rustler::Atom {
    let callback_result = decode_callback_result(result);
    resource.callbacks.respond(callback_id, callback_result);
    atoms::ok()
}

fn decode_callback_result(term: Term) -> CallbackResult {
    let Ok(tuple) = term.decode::<(rustler::Atom, Term)>() else {
        return CallbackResult::Err(
            "Invalid callback result: expected {atom, term} tuple".to_string(),
        );
    };

    if tuple.0 == atoms::ok() {
        match term_to_intermediate(tuple.1) {
            Ok(tv) => CallbackResult::Ok(tv),
            Err(e) => CallbackResult::Err(format!("Failed to convert callback result: {e}")),
        }
    } else if tuple.0 == atoms::error() {
        match tuple.1.decode::<String>() {
            Ok(reason) => CallbackResult::Err(reason),
            Err(_) => {
                CallbackResult::Err("Callback returned error with non-string reason".to_string())
            }
        }
    } else {
        CallbackResult::Err("Invalid callback result: expected :ok or :error tag".to_string())
    }
}

#[rustler::nif]
fn get_timeout(resource: ResourceArc<runtime::Runtime>) -> u64 {
    resource.timeout.as_millis() as u64
}

#[rustler::nif]
fn is_alive(resource: ResourceArc<runtime::Runtime>) -> bool {
    resource.alive.load(Ordering::Relaxed)
}

/// Cancel a specific eval by id. Scoped so a queued eval's deadline can
/// never interrupt the eval running ahead of it.
#[rustler::nif]
fn interrupt(resource: ResourceArc<runtime::Runtime>, eval_id: u64) -> rustler::Atom {
    resource.control.cancel(eval_id);
    atoms::ok()
}

#[rustler::nif(schedule = "DirtyIo")]
fn stop_runtime(resource: ResourceArc<runtime::Runtime>) -> rustler::Atom {
    resource.alive.store(false, Ordering::Relaxed);
    resource.control.stop();
    // Unblock any dispatch closure waiting on a callback response, so a
    // wedged or abandoned eval can't keep the worker from seeing Stop.
    resource.callbacks.close();
    let (tx, rx) = std::sync::mpsc::channel();
    let _ = resource.send(worker::Message::Stop(tx));
    let _ = rx.recv_timeout(resource.timeout);
    atoms::ok()
}

rustler::init!("Elixir.ExSafejs.Native");
