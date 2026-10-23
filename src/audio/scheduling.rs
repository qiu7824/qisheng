//! Blocking waits that retain an absolute deadline on native audio hosts.
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::Duration;

pub(super) fn timeout_slice(remaining: Duration) -> Duration {
    #[cfg(target_os = "macos")]
    {
        remaining.min(Duration::from_millis(2))
    }
    #[cfg(not(target_os = "macos"))]
    {
        remaining
    }
}

pub(super) fn wait_render_deadline(remaining: Duration) {
    // Short blocking intervals avoid large Darwin timer-coalescing delays.
    // The caller retains the absolute frame deadline and checks shutdown after
    // each wakeup. Idle and paused renderers use their ordinary longer wait.
    std::thread::park_timeout(timeout_slice(remaining));
}

pub(super) fn recv_timeout<T>(
    receiver: &Receiver<T>,
    timeout: Duration,
) -> Result<T, RecvTimeoutError> {
    #[cfg(not(target_os = "macos"))]
    {
        receiver.recv_timeout(timeout)
    }
    #[cfg(target_os = "macos")]
    {
        let Some(deadline) = std::time::Instant::now().checked_add(timeout) else {
            return receiver.recv().map_err(|_| RecvTimeoutError::Disconnected);
        };
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            match receiver.recv_timeout(timeout_slice(remaining)) {
                Err(RecvTimeoutError::Timeout) if std::time::Instant::now() < deadline => {}
                result => return result,
            }
        }
    }
}
