//! Bounded retirement of the actual catalog listener and its asset readers.

use super::*;
use std::sync::TryLockError;
use std::time::{Duration, Instant};

pub(super) struct CatalogServerOwner {
    thread: Option<thread::JoinHandle<()>>,
    panicked: bool,
}
impl CatalogServerOwner {
    pub(super) fn new(thread: thread::JoinHandle<()>) -> Self {
        Self {
            thread: Some(thread),
            panicked: false,
        }
    }
    /// Last-owner destruction still waits for its actual thread; callers use
    /// shutdown to observe a deadline while retaining this owner for retry.
    pub(super) fn join_on_drop(&mut self) {
        if let Some(thread) = self.thread.take() {
            self.panicked |= thread.join().is_err();
        }
    }
}

impl Catalog {
    /// Timeout retains the same JoinHandle. A positive result means the real
    /// listener thread joined and the catalog released its asset references.
    pub fn shutdown(&self, timeout: Duration) -> Result<bool> {
        self.inner.shutdown.store(true, Ordering::Release);
        let deadline = Instant::now()
            .checked_add(timeout)
            .unwrap_or_else(Instant::now);
        loop {
            let mut owner = match self.inner.server.try_lock() {
                Ok(owner) => owner,
                Err(TryLockError::Poisoned(error)) => error.into_inner(),
                Err(TryLockError::WouldBlock) => {
                    if Instant::now() >= deadline {
                        return Ok(false);
                    }
                    thread::park_timeout(Duration::from_millis(1));
                    continue;
                }
            };
            if owner
                .thread
                .as_ref()
                .is_some_and(|thread| thread.is_finished())
            {
                owner.join_on_drop();
            }
            if owner.thread.is_none() {
                let panicked = owner.panicked;
                drop(owner);
                let mut assets = match self.inner.assets.try_write() {
                    Ok(assets) => assets,
                    Err(TryLockError::Poisoned(error)) => error.into_inner(),
                    Err(TryLockError::WouldBlock) => {
                        if Instant::now() >= deadline {
                            return Ok(false);
                        }
                        thread::park_timeout(Duration::from_millis(1));
                        continue;
                    }
                };
                assets.clear();
                ensure!(!panicked, "Catalog listener panicked during retirement");
                return Ok(true);
            }
            drop(owner);
            if Instant::now() >= deadline {
                return Ok(false);
            }
            thread::park_timeout(Duration::from_millis(1));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn actual_catalog_join_closes_listener_and_releases_assets() {
        let catalog = Catalog::new().unwrap();
        let address = reqwest::Url::parse(catalog.base_url()).unwrap();
        let port = address.port().unwrap();
        assert!(catalog.shutdown(Duration::from_secs(3)).unwrap());
        assert!(catalog.inner.server.lock().unwrap().thread.is_none());
        assert!(catalog.inner.assets.read().unwrap().is_empty());
        let listener = TcpListener::bind(("127.0.0.1", port)).unwrap();
        drop(listener);
        assert!(catalog.shutdown(Duration::ZERO).unwrap());
    }
    #[test]
    fn joined_catalog_panic_is_retained_across_another_poll() {
        let catalog = Catalog::new().unwrap();
        assert!(catalog.shutdown(Duration::from_secs(3)).unwrap());
        *catalog.inner.server.lock().unwrap() = CatalogServerOwner::new(thread::spawn(|| {
            panic!("Actual catalog retirement panic fixture")
        }));
        assert!(catalog.shutdown(Duration::from_secs(3)).is_err());
        assert!(catalog.inner.server.lock().unwrap().thread.is_none());
        assert!(catalog.shutdown(Duration::ZERO).is_err());
    }
}
