//! Blocking media reads belong to the independent decoder worker, never the audio callback.
use super::cache::{CachePin, PublicationReadBoundary};
use super::transport::{MediaIoError, MediaIoErrorKind, StreamShared};
use std::io::{self, Read, Seek, SeekFrom};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
use symphonia::core::io::MediaSource;

pub struct StreamMediaSource {
    shared: Arc<StreamShared>,
    position: u64,
    seen_epoch: u64,
    pin: CachePin,
}

impl std::fmt::Debug for StreamMediaSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StreamMediaSource")
            .field("position", &self.position)
            .field("representation_epoch", &self.seen_epoch)
            .finish_non_exhaustive()
    }
}

impl StreamMediaSource {
    pub(crate) fn new(shared: Arc<StreamShared>) -> Self {
        let pin = shared.cache.pin(&shared.object, 0);
        Self {
            shared,
            position: 0,
            seen_epoch: 0,
            pin,
        }
    }
    pub fn position(&self) -> u64 {
        self.position
    }
}

impl Read for StreamMediaSource {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if output.is_empty() {
            return Ok(0);
        }
        let read_epoch = self.shared.read_epoch.load(Ordering::Acquire);
        let started = Instant::now();
        loop {
            if self.shared.cancelled() {
                return Err(
                    MediaIoError::new(MediaIoErrorKind::Cancelled, "audio read cancelled")
                        .io_error(),
                );
            }
            let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
            if state.epoch != self.seen_epoch || state.change_pending {
                return Err(MediaIoError::new(
                    MediaIoErrorKind::RepresentationChanged,
                    "audio representation changed",
                )
                .io_error());
            }
            if let Some(error) = &state.error {
                return Err(error.io_error());
            }
            if self.shared.read_epoch.load(Ordering::Acquire) != read_epoch {
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "audio read interrupted",
                ));
            }
            if state.ready && state.byte_len.is_some_and(|length| self.position >= length) {
                match self
                    .shared
                    .cache
                    .foreground_boundary(&self.shared.object, Duration::ZERO)
                {
                    PublicationReadBoundary::Ready if !state.eof_pending => return Ok(0),
                    PublicationReadBoundary::Ready => {
                        let remaining = self
                            .shared
                            .publication_read_timeout
                            .saturating_sub(started.elapsed());
                        if remaining.is_zero() {
                            drop(state);
                            let error = MediaIoError::new(
                                MediaIoErrorKind::Timeout,
                                "encoded audio EOF acknowledgement timed out",
                            );
                            if self.shared.error_if_current(
                                error.clone(),
                                self.seen_epoch,
                                read_epoch,
                            ) {
                                return Err(error.io_error());
                            }
                            continue;
                        }
                        let _guard = self
                            .shared
                            .changed
                            .wait_timeout(state, remaining.min(Duration::from_millis(25)))
                            .unwrap_or_else(|error| error.into_inner());
                        continue;
                    }
                    PublicationReadBoundary::Failed => {
                        drop(state);
                        let error = MediaIoError::new(
                            MediaIoErrorKind::Storage,
                            "write encoded audio cache failed",
                        );
                        if self
                            .shared
                            .error_if_current(error.clone(), self.seen_epoch, read_epoch)
                        {
                            return Err(error.io_error());
                        }
                        continue;
                    }
                    PublicationReadBoundary::Closing => {
                        return Err(MediaIoError::new(
                            MediaIoErrorKind::Cancelled,
                            "audio read cancelled",
                        )
                        .io_error());
                    }
                    PublicationReadBoundary::Pending => {
                        drop(state);
                        let remaining = self
                            .shared
                            .publication_read_timeout
                            .saturating_sub(started.elapsed());
                        if remaining.is_zero() {
                            let error = MediaIoError::new(
                                MediaIoErrorKind::Timeout,
                                "encoded audio publication acknowledgement timed out",
                            );
                            if self.shared.error_if_current(
                                error.clone(),
                                self.seen_epoch,
                                read_epoch,
                            ) {
                                return Err(error.io_error());
                            }
                            continue;
                        }
                        self.shared.cache.foreground_boundary(
                            &self.shared.object,
                            remaining.min(Duration::from_millis(25)),
                        );
                        continue;
                    }
                }
            }
            if state.ready
                && !state.range
                && self.position < state.sequential_until
                && self.shared.cache.try_contains(
                    &self.shared.object,
                    self.position / self.shared.block_size * self.shared.block_size,
                ) == Some(false)
            {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "sequential audio position has left the bounded cache",
                ));
            }
            drop(state);
            // Cache reads may perform disk syscalls on this decoder thread only.
            match self
                .shared
                .cache
                .try_read(&self.shared.object, self.position, output)
            {
                Ok(Some(count)) => {
                    let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
                    if state.epoch != self.seen_epoch || state.change_pending {
                        return Err(MediaIoError::new(
                            MediaIoErrorKind::RepresentationChanged,
                            "audio representation changed",
                        )
                        .io_error());
                    }
                    if self.shared.read_epoch.load(Ordering::Acquire) != read_epoch {
                        return Err(io::Error::new(
                            io::ErrorKind::Interrupted,
                            "audio read interrupted",
                        ));
                    }
                    self.position = self.position.saturating_add(count as u64);
                    state.cursor = self.position;
                    let offset = self.position / self.shared.block_size * self.shared.block_size;
                    self.pin = self.shared.cache.pin(&self.shared.object, offset);
                    drop(state);
                    self.shared.wake();
                    return Ok(count);
                }
                Err(_) => {
                    let error = MediaIoError::new(
                        MediaIoErrorKind::Storage,
                        "read encoded audio cache failed",
                    );
                    self.shared.error(error.clone());
                    return Err(error.io_error());
                }
                Ok(None) => (),
            }
            state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
            if self.shared.read_epoch.load(Ordering::Acquire) != read_epoch {
                continue;
            }
            if state.range || !state.ready {
                state.wanted =
                    Some(self.position / self.shared.block_size * self.shared.block_size);
            }
            state.cursor = self.position;
            self.shared.notify.notify_one();
            let _guard = self
                .shared
                .changed
                .wait_timeout(state, Duration::from_millis(25))
                .unwrap_or_else(|e| e.into_inner());
        }
    }
}

impl Seek for StreamMediaSource {
    fn seek(&mut self, seek: SeekFrom) -> io::Result<u64> {
        if self.shared.cancelled() {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "audio seek cancelled",
            ));
        }
        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        let next = match seek {
            SeekFrom::Start(position) => position,
            SeekFrom::Current(delta) => {
                self.position.checked_add_signed(delta).ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidInput, "audio seek overflow")
                })?
            }
            SeekFrom::End(delta) => state
                .byte_len
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::Unsupported,
                        "unknown-length audio cannot seek from end",
                    )
                })?
                .checked_add_signed(delta)
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidInput, "audio seek overflow")
                })?,
        };
        if (state.change_pending || state.epoch != self.seen_epoch) && next != 0 {
            return Err(MediaIoError::new(
                MediaIoErrorKind::RepresentationChanged,
                "changed audio representation must restart at zero",
            )
            .io_error());
        }
        if state.ready
            && !state.range
            && next < state.sequential_until
            && !state.byte_len.is_some_and(|length| next == length)
            && self.shared.cache.try_contains(
                &self.shared.object,
                next / self.shared.block_size * self.shared.block_size,
            ) == Some(false)
        {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "sequential audio seek is outside the bounded cache",
            ));
        }
        if next == 0 {
            self.seen_epoch = state.epoch;
            state.change_pending = false;
        }
        self.position = next;
        state.cursor = next;
        self.pin = self.shared.cache.pin(
            &self.shared.object,
            next / self.shared.block_size * self.shared.block_size,
        );
        if state.range || !state.ready {
            state.wanted = Some(next / self.shared.block_size * self.shared.block_size);
        }
        // A decoder may start consuming the exact lookahead already in flight.
        // Keep that response and its DATA budget reservation; other seeks must
        // still supersede pending work, including ordinary demand requests.
        let keep_lookahead = state.lookahead_in_flight.is_some_and(|active| {
            active.offset == next / self.shared.block_size * self.shared.block_size
                && active.read_epoch == self.shared.read_epoch.load(Ordering::Acquire)
                && active.representation_epoch == state.epoch
        });
        if !keep_lookahead {
            self.shared.read_epoch.fetch_add(1, Ordering::AcqRel);
        }
        drop(state);
        self.shared.wake();
        Ok(next)
    }
}

impl MediaSource for StreamMediaSource {
    fn is_seekable(&self) -> bool {
        self.shared
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .range
    }
    fn byte_len(&self) -> Option<u64> {
        self.shared
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .byte_len
    }
}

impl Drop for StreamMediaSource {
    fn drop(&mut self) {
        self.shared.cancel.store(true, Ordering::Release);
        self.shared.wake();
    }
}
