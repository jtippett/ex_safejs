use crate::convert::CallbackResult;
use crate::worker;
use rustler::{Encoder, LocalPid, OwnedEnv};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

pub struct CallbackRegistry {
    pending: Mutex<HashMap<u64, mpsc::Sender<CallbackResult>>>,
    next_id: AtomicU64,
}

impl CallbackRegistry {
    pub fn new() -> Self {
        Self {
            pending: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
        }
    }

    pub fn register(&self) -> (u64, mpsc::Receiver<CallbackResult>) {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::channel();
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id, tx);
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

    pub fn clear(&self) {
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }
}

pub struct Runtime {
    sender: mpsc::Sender<worker::Message>,
    pub callbacks: Arc<CallbackRegistry>,
    pub interrupt: Arc<AtomicBool>,
    pub alive: Arc<AtomicBool>,
    pub timeout: Duration,
}

impl Runtime {
    pub fn new(
        sender: mpsc::Sender<worker::Message>,
        interrupt: Arc<AtomicBool>,
        timeout: Duration,
    ) -> Self {
        Self {
            sender,
            callbacks: Arc::new(CallbackRegistry::new()),
            interrupt,
            alive: Arc::new(AtomicBool::new(true)),
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
        self.interrupt.store(true, Ordering::Relaxed);
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
