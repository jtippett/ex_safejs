mod atoms;
mod convert;
mod runtime;
mod worker;

use convert::{term_to_intermediate, CallbackResult, JsResult, JsValue};
use rustler::{Env, ResourceArc, Term};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

#[rustler::nif]
fn start_runtime(
    env: Env,
    start_ref: Term,
    timeout_ms: u64,
    memory_limit: usize,
    max_stack_size: usize,
) -> rustler::Atom {
    let task_pid = env.pid();
    let (sender, receiver) = std::sync::mpsc::channel::<worker::Message>();
    let interrupt = Arc::new(AtomicBool::new(false));
    let interrupt_worker = Arc::clone(&interrupt);
    let timeout = Duration::from_millis(timeout_ms);

    // Save the ref so we can tag the reply message
    let mut ref_env = rustler::OwnedEnv::new();
    let saved_ref = ref_env.save(start_ref);

    let opts = worker::WorkerOpts {
        memory_limit,
        max_stack_size,
    };

    std::thread::spawn(move || match worker::Worker::new(opts, interrupt_worker) {
        Ok(mut w) => {
            let sent = ref_env.send_and_clear(&task_pid, |env| {
                let ref_term = saved_ref.load(env);
                (
                    atoms::ex_safejs_start(),
                    ref_term,
                    (
                        atoms::ok(),
                        ResourceArc::new(runtime::Runtime::new(sender, interrupt, timeout)),
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
    });

    atoms::ok()
}

#[rustler::nif(schedule = "DirtyIo")]
fn eval_sync(resource: ResourceArc<runtime::Runtime>, code: String) -> JsResult {
    let (tx, rx) = std::sync::mpsc::channel();
    if resource.send(worker::Message::Eval(code, tx)).is_err() {
        return JsResult::Err("dead_runtime".to_string());
    }

    match rx.recv_timeout(resource.timeout) {
        Ok(Ok(val)) => JsResult::Ok(val),
        Ok(Err(err)) => JsResult::Err(err),
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            resource.interrupt.store(true, Ordering::Relaxed);
            JsResult::Err("timeout".to_string())
        }
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            JsResult::Err("dead_runtime".to_string())
        }
    }
}

#[rustler::nif]
fn eval_with_callbacks(
    env: Env,
    resource: ResourceArc<runtime::Runtime>,
    code: String,
    fn_names: Vec<String>,
) -> JsResult {
    let pid = env.pid();
    let callbacks = Arc::clone(&resource.callbacks);
    let timeout = resource.timeout;
    if resource
        .send(worker::Message::EvalWithCallbacks(
            code, fn_names, callbacks, pid, timeout,
        ))
        .is_err()
    {
        return JsResult::Err("dead_runtime".to_string());
    }
    JsResult::Ok(JsValue::Null)
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

#[rustler::nif]
fn interrupt(resource: ResourceArc<runtime::Runtime>) -> rustler::Atom {
    resource.interrupt.store(true, Ordering::Relaxed);
    atoms::ok()
}

#[rustler::nif(schedule = "DirtyIo")]
fn stop_runtime(resource: ResourceArc<runtime::Runtime>) -> rustler::Atom {
    resource.alive.store(false, Ordering::Relaxed);
    resource.interrupt.store(true, Ordering::Relaxed);
    let (tx, rx) = std::sync::mpsc::channel();
    let _ = resource.send(worker::Message::Stop(tx));
    let _ = rx.recv_timeout(resource.timeout);
    atoms::ok()
}

rustler::init!("Elixir.ExSafejs.Native");
