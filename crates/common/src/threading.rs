use anyhow::{Result, anyhow};
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

#[derive(Default)]
struct ThreadRegistryInner {
    next_id: AtomicUsize,
    threads: Mutex<HashMap<usize, ThreadInfo>>,
}

struct ThreadInfo {
    name: String,
}

#[derive(Clone, Default)]
pub struct ThreadRegistry {
    inner: Arc<ThreadRegistryInner>,
}

impl ThreadRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn spawn<F>(&self, name: impl Into<String>, f: F) -> Result<ThreadHandle>
    where
        F: FnOnce() + Send + 'static,
    {
        let name = name.into();
        let builder = thread::Builder::new().name(name.clone());
        let join_handle = builder
            .spawn(f)
            .map_err(|e| anyhow!("failed to spawn thread '{name}': {e}"))?;

        let id = self.inner.next_id.fetch_add(1, Ordering::SeqCst);
        self.inner
            .threads
            .lock()
            .expect("thread registry mutex poisoned")
            .insert(id, ThreadInfo { name: name.clone() });

        Ok(ThreadHandle {
            name,
            id,
            handle: Some(join_handle),
            inner: Arc::clone(&self.inner),
        })
    }

    pub fn active_thread_names(&self) -> Vec<String> {
        self.inner
            .threads
            .lock()
            .expect("thread registry mutex poisoned")
            .values()
            .map(|info| info.name.clone())
            .collect()
    }
}

pub struct ThreadHandle {
    name: String,
    id: usize,
    handle: Option<JoinHandle<()>>,
    inner: Arc<ThreadRegistryInner>,
}

impl ThreadHandle {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn join(mut self) -> std::thread::Result<()> {
        self.inner
            .threads
            .lock()
            .expect("thread registry mutex poisoned")
            .remove(&self.id);
        if let Some(handle) = self.handle.take() {
            handle.join()
        } else {
            Ok(())
        }
    }
}

impl Drop for ThreadHandle {
    fn drop(&mut self) {
        let _ = self
            .inner
            .threads
            .lock()
            .expect("thread registry mutex poisoned")
            .remove(&self.id);
        // Dropping the JoinHandle detaches the thread; we intentionally do not block here.
    }
}
