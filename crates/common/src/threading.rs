use anyhow::{Result, anyhow};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ThreadStatus {
    Reserved,
    Running,
    Joining,
    Joined,
    Detached,
    Panicked,
}

#[derive(Clone, Debug)]
pub struct ThreadSnapshot {
    pub id: usize,
    pub name: String,
    pub status: ThreadStatus,
}

struct ThreadRecord {
    id: usize,
    name: String,
    status: ThreadStatus,
    handle: Option<JoinHandle<()>>,
}

impl ThreadRecord {
    fn snapshot(&self) -> ThreadSnapshot {
        ThreadSnapshot {
            id: self.id,
            name: self.name.clone(),
            status: self.status.clone(),
        }
    }

    fn is_active(&self) -> bool {
        matches!(self.status, ThreadStatus::Running | ThreadStatus::Joining)
    }
}

#[derive(Default)]
struct ThreadRegistryInner {
    next_id: AtomicUsize,
    records: Mutex<Vec<ThreadRecord>>,
}

#[derive(Clone, Default)]
pub struct ThreadRegistry {
    inner: Arc<ThreadRegistryInner>,
}

impl ThreadRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_slots<I, S>(slots: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let registry = Self::new();
        registry.reserve_slots(slots);
        registry
    }

    pub fn reserve_slots<I, S>(&self, slots: I)
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let inner = &self.inner;
        let mut records = inner
            .records
            .lock()
            .expect("thread registry mutex poisoned");
        for name in slots {
            let name = name.into();
            if records.iter().any(|record| record.name == name) {
                continue;
            }
            let id = inner.next_id.fetch_add(1, Ordering::SeqCst);
            records.push(ThreadRecord {
                id,
                name,
                status: ThreadStatus::Reserved,
                handle: None,
            });
        }
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

        let mut join_handle_opt = Some(join_handle);
        let mut records = self
            .inner
            .records
            .lock()
            .expect("thread registry mutex poisoned");

        let id = if let Some(record) = records
            .iter_mut()
            .find(|record| record.name == name && record.handle.is_none())
        {
            record.status = ThreadStatus::Running;
            record.handle = join_handle_opt.take();
            record.id
        } else {
            let id = self.inner.next_id.fetch_add(1, Ordering::SeqCst);
            records.push(ThreadRecord {
                id,
                name: name.clone(),
                status: ThreadStatus::Running,
                handle: join_handle_opt.take(),
            });
            id
        };

        Ok(ThreadHandle {
            id,
            name,
            inner: Arc::clone(&self.inner),
        })
    }

    pub fn active_thread_names(&self) -> Vec<String> {
        let records = self
            .inner
            .records
            .lock()
            .expect("thread registry mutex poisoned");
        records
            .iter()
            .filter(|record| record.is_active())
            .map(|record| record.name.clone())
            .collect()
    }

    pub fn active_count(&self) -> usize {
        let records = self
            .inner
            .records
            .lock()
            .expect("thread registry mutex poisoned");
        records.iter().filter(|record| record.is_active()).count()
    }

    pub fn snapshot(&self) -> Vec<ThreadSnapshot> {
        let records = self
            .inner
            .records
            .lock()
            .expect("thread registry mutex poisoned");
        records.iter().map(ThreadRecord::snapshot).collect()
    }
}

pub struct ThreadHandle {
    id: usize,
    name: String,
    inner: Arc<ThreadRegistryInner>,
}

impl ThreadHandle {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn join(self) -> std::thread::Result<()> {
        let handle = {
            let mut records = self
                .inner
                .records
                .lock()
                .expect("thread registry mutex poisoned");
            if let Some(record) = records.iter_mut().find(|record| record.id == self.id) {
                record.status = ThreadStatus::Joining;
                record.handle.take()
            } else {
                None
            }
        };

        let join_result = if let Some(handle) = handle {
            handle.join()
        } else {
            Ok(())
        };

        let mut records = self
            .inner
            .records
            .lock()
            .expect("thread registry mutex poisoned");
        if let Some(record) = records.iter_mut().find(|record| record.id == self.id) {
            record.status = if join_result.is_ok() {
                ThreadStatus::Joined
            } else {
                ThreadStatus::Panicked
            };
        }

        join_result
    }
}

impl Drop for ThreadHandle {
    fn drop(&mut self) {
        let handle = {
            let mut records = self
                .inner
                .records
                .lock()
                .expect("thread registry mutex poisoned");
            if let Some(record) = records.iter_mut().find(|record| record.id == self.id) {
                if record.handle.is_some() {
                    record.status = ThreadStatus::Detached;
                }
                record.handle.take()
            } else {
                None
            }
        };
        drop(handle);
    }
}
