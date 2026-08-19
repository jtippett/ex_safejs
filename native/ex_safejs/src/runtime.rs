use crate::convert::CallbackResult;
use crate::worker;
use rustler::{Encoder, LocalPid, OwnedEnv};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

/// Eval-scoped interruption. The interrupt handler, the worker, and the
/// dispatch closures all consult this instead of a bare `AtomicBool`, so an
/// interrupt names *which* eval it cancels. Without the identity, a queued
/// eval's deadline could abort the eval running ahead of it, or fire before
/// its own eval is dequeued and then be silently cleared and lost — leaving
/// a core spinning uninterruptibly.
pub struct EvalControl {
    /// eval_id currently executing on the worker thread; 0 when idle.
    current: AtomicU64,
    /// Most recently cancelled eval_id (set by the Elixir deadline).
    cancelled: AtomicU64,
    /// Hard stop from `stop/1` or `Drop`: cancels whatever is running and
    /// anything that follows.
    stopping: AtomicBool,
}

impl EvalControl {
    pub fn new() -> Self {
        Self {
            current: AtomicU64::new(0),
            cancelled: AtomicU64::new(0),
            stopping: AtomicBool::new(false),
        }
    }

    /// True when the currently-running eval should be interrupted. Read on
    /// every QuickJS interrupt tick, so kept to two relaxed loads.
    pub fn should_interrupt(&self) -> bool {
        if self.stopping.load(Ordering::Relaxed) {
            return true;
        }
        let c = self.current.load(Ordering::Relaxed);
        c != 0 && self.cancelled.load(Ordering::Relaxed) == c
    }

    /// True when this specific eval has been cancelled (or the runtime is
    /// stopping). Used by the worker before running a dequeued eval and by a
    /// blocked dispatch to decide whether to abandon its wait.
    pub fn is_cancelled(&self, eval_id: u64) -> bool {
        self.stopping.load(Ordering::Relaxed) || self.cancelled.load(Ordering::Relaxed) == eval_id
    }

    /// True while `eval_id` is the eval the worker is currently running. A
    /// dispatch whose captured eval is no longer current is a stale reference
    /// retained across evals.
    pub fn is_running(&self, eval_id: u64) -> bool {
        self.current.load(Ordering::Relaxed) == eval_id
    }

    pub fn enter(&self, eval_id: u64) {
        self.current.store(eval_id, Ordering::Relaxed);
    }

    pub fn leave(&self) {
        self.current.store(0, Ordering::Relaxed);
    }

    pub fn cancel(&self, eval_id: u64) {
        self.cancelled.store(eval_id, Ordering::Relaxed);
    }

    pub fn stop(&self) {
        self.stopping.store(true, Ordering::Relaxed);
    }
}

pub struct CallbackRegistry {
    pending: Mutex<HashMap<u64, mpsc::Sender<CallbackResult>>>,
    next_id: AtomicU64,
    closed: AtomicBool,
}

impl CallbackRegistry {
    pub fn new() -> Self {
        Self {
            pending: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            closed: AtomicBool::new(false),
        }
    }

    pub fn register(&self) -> (u64, mpsc::Receiver<CallbackResult>) {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::channel();
        // After close(), don't retain the sender: the receiver disconnects
        // immediately, so a dispatch racing with shutdown errors out instead
        // of blocking on a response that will never come.
        if !self.closed.load(Ordering::Relaxed) {
            self.pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(id, tx);
        }
        (id, rx)
    }

    pub fn respond(&self, id: u64, result: CallbackResult) -> bool {
        if let Some(tx) = self
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&id)
        {
            tx.send(result).is_ok()
        } else {
            false
        }
    }

    /// Per-eval cleanup: drop any unanswered entries so a stale respond
    /// can't cross into a later eval. The registry stays usable.
    pub fn clear(&self) {
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }

    /// Shutdown: disconnect every waiting dispatch and refuse to retain
    /// senders for future registrations. Irreversible per registry.
    pub fn close(&self) {
        self.closed.store(true, Ordering::Relaxed);
        self.clear();
    }
}

pub struct Runtime {
    sender: mpsc::Sender<worker::Message>,
    pub callbacks: Arc<CallbackRegistry>,
    pub control: Arc<EvalControl>,
    pub alive: Arc<AtomicBool>,
    pub timeout: Duration,
}

impl Runtime {
    pub fn new(
        sender: mpsc::Sender<worker::Message>,
        control: Arc<EvalControl>,
        alive: Arc<AtomicBool>,
        callbacks: Arc<CallbackRegistry>,
        timeout: Duration,
    ) -> Self {
        Self {
            sender,
            callbacks,
            control,
            alive,
            timeout,
        }
    }

    pub fn send(&self, msg: worker::Message) -> Result<(), mpsc::SendError<worker::Message>> {
        self.sender.send(msg)
    }
}

#[rustler::resource_impl]
impl rustler::Resource for Runtime {}

impl Drop for Runtime {
    fn drop(&mut self) {
        self.alive.store(false, Ordering::Relaxed);
        self.control.stop();
        // Disconnect any dispatch closure blocked on a callback response —
        // e.g. when the Elixir process servicing the eval died — so the
        // worker can unwind and read the Stop message instead of leaking.
        self.callbacks.close();
        let (tx, _rx) = mpsc::channel();
        let _ = self.sender.send(worker::Message::Stop(tx));
    }
}

pub fn send_to_pid<T>(pid: &LocalPid, data: T) -> bool
where
    T: Encoder,
{
    OwnedEnv::new().send_and_clear(pid, |_env| data).is_ok()
}
