//! Shared hold on a device. The first session on a simulator or emulator
//! may boot it and start WebDriverAgent; later sessions reuse that, and
//! whatever the first one set up is undone when the last one ends.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use anyhow::Result;

/// What undoes a device's setup once nobody holds it.
pub type Cleanup = Box<dyn FnOnce() + Send>;

struct Slot<T> {
    count: usize,
    value: T,
    cleanup: Option<Cleanup>,
}

pub struct Leases<T> {
    map: Arc<Mutex<HashMap<String, Slot<T>>>>,
}

impl<T> Default for Leases<T> {
    fn default() -> Self {
        Self {
            map: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

impl<T: Clone + Send + 'static> Leases<T> {
    /// A hold on `key`, set up by `make` when nobody holds it yet. `make`
    /// returns the shared value and what to do once the last hold goes.
    /// It runs under the lock, so two sessions never boot the same device.
    pub fn acquire(
        &self,
        key: &str,
        make: impl FnOnce() -> Result<(T, Option<Cleanup>)>,
    ) -> Result<Lease<T>> {
        let mut map = self.map.lock().unwrap();
        if let Some(slot) = map.get_mut(key) {
            slot.count += 1;
            return Ok(Lease {
                key: key.to_string(),
                value: slot.value.clone(),
                map: self.map.clone(),
            });
        }
        let (value, cleanup) = make()?;
        map.insert(
            key.to_string(),
            Slot {
                count: 1,
                value: value.clone(),
                cleanup,
            },
        );
        Ok(Lease {
            key: key.to_string(),
            value,
            map: self.map.clone(),
        })
    }

    #[cfg(test)]
    pub fn held(&self, key: &str) -> bool {
        self.map.lock().unwrap().contains_key(key)
    }
}

pub struct Lease<T> {
    key: String,
    pub value: T,
    map: Arc<Mutex<HashMap<String, Slot<T>>>>,
}

impl<T> Drop for Lease<T> {
    fn drop(&mut self) {
        let cleanup = {
            let mut map = self.map.lock().unwrap();
            let Some(slot) = map.get_mut(&self.key) else {
                return;
            };
            slot.count -= 1;
            if slot.count > 0 {
                return;
            }
            map.remove(&self.key).and_then(|s| s.cleanup)
        };
        // Outside the lock: shutting a device down takes a while.
        if let Some(f) = cleanup {
            f();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn last_one_cleans_up() {
        let leases = Leases::<u16>::default();
        let made = Arc::new(AtomicUsize::new(0));
        let cleaned = Arc::new(AtomicUsize::new(0));
        let make = || {
            made.fetch_add(1, Ordering::SeqCst);
            let c = cleaned.clone();
            Ok((
                8100,
                Some(Box::new(move || {
                    c.fetch_add(1, Ordering::SeqCst);
                }) as Cleanup),
            ))
        };
        let a = leases.acquire("sim", make).unwrap();
        let b = leases.acquire("sim", || unreachable!()).unwrap();
        assert_eq!((a.value, b.value), (8100, 8100));
        drop(a);
        assert_eq!(cleaned.load(Ordering::SeqCst), 0);
        drop(b);
        assert_eq!(cleaned.load(Ordering::SeqCst), 1);
        assert_eq!(made.load(Ordering::SeqCst), 1);
        assert!(!leases.held("sim"));
    }
}
