use super::{
    SystemAction, SystemInputSender, SystemPlaybackState, SystemProjection, types::SourceArtworkPin,
};
use anyhow::{Result, ensure};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use windows::{
    Foundation::{TimeSpan, TypedEventHandler},
    Media::{
        MediaPlaybackStatus, MediaPlaybackType, PlaybackPositionChangeRequestedEventArgs,
        PlaybackRateChangeRequestedEventArgs, SystemMediaTransportControls,
        SystemMediaTransportControlsButton, SystemMediaTransportControlsButtonPressedEventArgs,
        SystemMediaTransportControlsTimelineProperties,
    },
    Storage::Streams::{
        DataWriter, DataWriterStoreOperation, InMemoryRandomAccessStream,
        RandomAccessStreamReference,
    },
    Win32::{Foundation::HWND, System::WinRT::ISystemMediaTransportControlsInterop},
    core::{HSTRING, factory},
};

struct CallbackSet {
    generation: u64,
    closing: Arc<AtomicBool>,
    buttons: Option<i64>,
    position: Option<i64>,
    rate: Option<i64>,
}
struct ThumbnailStore {
    stream: InMemoryRandomAccessStream,
    writer: DataWriter,
    operation: DataWriterStoreOperation,
    expected_bytes: u32,
    stored: bool,
    detached: bool,
    operation_closed: bool,
    writer_closed: bool,
    stream_closed: bool,
    cancelling: bool,
}
pub(super) struct SmtcOwner {
    controls: SystemMediaTransportControls,
    callbacks: Option<CallbackSet>,
    artwork_pin: Option<SourceArtworkPin>,
    pending_artwork: Option<SourceArtworkPin>,
    store: Option<ThumbnailStore>,
}
impl SmtcOwner {
    pub(super) fn create(hwnd: *mut core::ffi::c_void) -> Result<Self> {
        let factory: ISystemMediaTransportControlsInterop =
            factory::<SystemMediaTransportControls, ISystemMediaTransportControlsInterop>()?;
        let controls: SystemMediaTransportControls = unsafe { factory.GetForWindow(HWND(hwnd))? };
        Ok(Self {
            controls,
            callbacks: None,
            artwork_pin: None,
            pending_artwork: None,
            store: None,
        })
    }
    fn retire_store(&mut self) -> Result<()> {
        let Some(store) = &mut self.store else {
            return Ok(());
        };
        if !store.operation_closed {
            if store.operation.Status()?.0 == 0 && !store.cancelling {
                store.operation.Cancel()?;
                store.cancelling = true;
            }
            // Official windows-future AsyncStatus: Started=0. Cancel requests
            // retirement; it does not establish that the operation has ended.
            ensure!(
                store.operation.Status()?.0 != 0,
                "Native thumbnail store cancellation remains pending"
            );
            store.operation.Close()?;
            store.operation_closed = true;
        }
        if !store.writer_closed {
            store.writer.Close()?;
            store.writer_closed = true;
        }
        if !store.stream_closed {
            store.stream.Close()?;
            store.stream_closed = true;
        }
        self.store = None;
        Ok(())
    }
    fn prepare_thumbnail(&mut self) -> Result<RandomAccessStreamReference> {
        let pin = self
            .pending_artwork
            .as_ref()
            .expect("retained thumbnail writer pin");
        if self.store.is_none() {
            let stream = InMemoryRandomAccessStream::new()?;
            let writer = DataWriter::CreateDataWriter(&stream)?;
            writer.WriteBytes(pin.bytes())?;
            let operation = writer.StoreAsync()?;
            self.store = Some(ThumbnailStore {
                stream,
                writer,
                operation,
                expected_bytes: pin.bytes().len() as u32,
                stored: false,
                detached: false,
                operation_closed: false,
                writer_closed: false,
                stream_closed: false,
                cancelling: false,
            });
        }
        let store = self
            .store
            .as_mut()
            .expect("retained native store operation");
        ensure!(!store.cancelling, "Native thumbnail store is retiring");
        if !store.stored {
            ensure!(
                store.operation.Status()?.0 != 0,
                "Native thumbnail store is pending"
            );
            // GetResults only runs after a terminal AsyncStatus. Failed/canceled
            // operations remain owned for the normal retirement path.
            ensure!(
                store.operation.GetResults()? == store.expected_bytes,
                "Native thumbnail store wrote a different byte count"
            );
            store.stored = true;
        }
        if !store.detached {
            store.writer.DetachStream()?;
            store.detached = true;
        }
        if !store.operation_closed {
            store.operation.Close()?;
            store.operation_closed = true;
        }
        if !store.writer_closed {
            store.writer.Close()?;
            store.writer_closed = true;
        }
        store.stream.Seek(0)?;
        Ok(RandomAccessStreamReference::CreateFromStream(
            &store.stream,
        )?)
    }
    fn retire_callbacks(&mut self) -> Result<()> {
        let Some(callbacks) = &mut self.callbacks else {
            return Ok(());
        };
        callbacks.closing.store(true, Ordering::Release);
        let mut failure = None;
        if let Some(token) = callbacks.buttons {
            match self.controls.RemoveButtonPressed(token) {
                Ok(()) => callbacks.buttons = None,
                Err(error) => failure = Some(error),
            }
        }
        if let Some(token) = callbacks.position {
            match self.controls.RemovePlaybackPositionChangeRequested(token) {
                Ok(()) => callbacks.position = None,
                Err(error) => failure = Some(error),
            }
        }
        if let Some(token) = callbacks.rate {
            match self.controls.RemovePlaybackRateChangeRequested(token) {
                Ok(()) => callbacks.rate = None,
                Err(error) => failure = Some(error),
            }
        }
        if let Some(error) = failure {
            return Err(error.into());
        }
        self.callbacks = None;
        Ok(())
    }
    fn bind_callbacks(&mut self, generation: u64, sender: &SystemInputSender) -> Result<()> {
        if self.callbacks.as_ref().is_some_and(|c| {
            c.generation == generation
                && !c.closing.load(Ordering::Acquire)
                && c.buttons.is_some()
                && c.position.is_some()
                && c.rate.is_some()
        }) {
            return Ok(());
        }
        self.retire_callbacks()?;
        let closing = Arc::new(AtomicBool::new(false));
        self.callbacks = Some(CallbackSet {
            generation,
            closing: closing.clone(),
            buttons: None,
            position: None,
            rate: None,
        });
        let input = sender.clone();
        let gate = closing.clone();
        let buttons = TypedEventHandler::<
            SystemMediaTransportControls,
            SystemMediaTransportControlsButtonPressedEventArgs,
        >::new(move |_, args| {
            if gate.load(Ordering::Acquire) {
                return Ok(());
            }
            let Some(args) = args.as_ref() else {
                return Ok(());
            };
            let button = args.Button()?;
            let action = match button {
                SystemMediaTransportControlsButton::Play => Some(SystemAction::Play),
                SystemMediaTransportControlsButton::Pause => Some(SystemAction::Pause),
                SystemMediaTransportControlsButton::Stop => Some(SystemAction::Stop),
                SystemMediaTransportControlsButton::Previous => Some(SystemAction::Previous),
                SystemMediaTransportControlsButton::Next => Some(SystemAction::Next),
                SystemMediaTransportControlsButton::FastForward => {
                    Some(SystemAction::SeekRelative { offset_ms: 5000 })
                }
                SystemMediaTransportControlsButton::Rewind => {
                    Some(SystemAction::SeekRelative { offset_ms: -5000 })
                }
                _ => None,
            };
            if let Some(action) = action {
                let fence = action.requires_generation().then_some(generation);
                let _ = input.try_submit(action, fence);
            }
            Ok(())
        });
        self.callbacks.as_mut().unwrap().buttons = Some(self.controls.ButtonPressed(&buttons)?);
        let input = sender.clone();
        let gate = closing.clone();
        let position = TypedEventHandler::<
            SystemMediaTransportControls,
            PlaybackPositionChangeRequestedEventArgs,
        >::new(move |_, args| {
            if gate.load(Ordering::Acquire) {
                return Ok(());
            }
            if let Some(args) = args.as_ref() {
                let ticks = args.RequestedPlaybackPosition()?.Duration;
                if ticks >= 0 {
                    let _ = input.try_submit(
                        SystemAction::SeekAbsolute {
                            position_ms: ticks as u64 / 10_000,
                        },
                        Some(generation),
                    );
                }
            }
            Ok(())
        });
        self.callbacks.as_mut().unwrap().position =
            Some(self.controls.PlaybackPositionChangeRequested(&position)?);
        let input = sender.clone();
        let gate = closing;
        let rate = TypedEventHandler::<
            SystemMediaTransportControls,
            PlaybackRateChangeRequestedEventArgs,
        >::new(move |_, args| {
            if gate.load(Ordering::Acquire) {
                return Ok(());
            }
            if let Some(args) = args.as_ref() {
                let _ = input.try_submit(
                    SystemAction::PlaybackRate {
                        rate: args.RequestedPlaybackRate()?,
                    },
                    Some(generation),
                );
            }
            Ok(())
        });
        self.callbacks.as_mut().unwrap().rate =
            Some(self.controls.PlaybackRateChangeRequested(&rate)?);
        Ok(())
    }
    pub(super) fn update(
        &mut self,
        value: &SystemProjection,
        sender: &SystemInputSender,
    ) -> Result<()> {
        value.validate()?;
        self.bind_callbacks(value.generation, sender)?;
        let active = value.media_id.is_some();
        self.controls.SetIsEnabled(active)?;
        self.controls.SetIsPlayEnabled(active)?;
        self.controls.SetIsPauseEnabled(active)?;
        self.controls.SetIsStopEnabled(active)?;
        self.controls.SetIsPreviousEnabled(value.can_previous)?;
        self.controls.SetIsNextEnabled(value.can_next)?;
        self.controls
            .SetIsFastForwardEnabled(active && value.can_seek)?;
        self.controls.SetIsRewindEnabled(active && value.can_seek)?;
        self.controls.SetPlaybackStatus(match value.state {
            SystemPlaybackState::None => MediaPlaybackStatus::Closed,
            SystemPlaybackState::Stopped => MediaPlaybackStatus::Stopped,
            SystemPlaybackState::Paused => MediaPlaybackStatus::Paused,
            SystemPlaybackState::Buffering => MediaPlaybackStatus::Changing,
            SystemPlaybackState::Playing => MediaPlaybackStatus::Playing,
        })?;
        self.controls.SetPlaybackRate(value.playback_rate)?;
        let timeline = SystemMediaTransportControlsTimelineProperties::new()?;
        let span = |ms: u64| TimeSpan {
            Duration: (ms * 10_000) as i64,
        };
        timeline.SetStartTime(span(0))?;
        timeline.SetMinSeekTime(span(0))?;
        timeline.SetEndTime(span(value.duration_ms))?;
        timeline.SetMaxSeekTime(span(value.duration_ms))?;
        timeline.SetPosition(span(value.position_ms))?;
        self.controls.UpdateTimelineProperties(&timeline)?;
        let display = self.controls.DisplayUpdater()?;
        display.SetType(MediaPlaybackType::Music)?;
        let music = display.MusicProperties()?;
        music.SetTitle(&HSTRING::from(&value.title))?;
        music.SetArtist(&HSTRING::from(&value.artist))?;
        music.SetAlbumTitle(&HSTRING::from(&value.album))?;
        let desired = value.artwork.as_ref().filter(|a| a.is_current());
        // An ambiguous failed Update retains both real pins. Clear the actual
        // thumbnail before admitting a different third candidate.
        if self
            .pending_artwork
            .as_ref()
            .is_some_and(|pin| desired.is_none_or(|a| !pin.matches(a)))
        {
            self.retire_store()?;
            display.SetThumbnail(None::<&RandomAccessStreamReference>)?;
            display.Update()?;
            self.pending_artwork = None;
            self.artwork_pin = None;
        }
        let unchanged = self.pending_artwork.is_none()
            && match (&self.artwork_pin, desired) {
                (None, None) => true,
                (Some(pin), Some(artwork)) => pin.matches(artwork),
                _ => false,
            };
        if !unchanged {
            if let Some(artwork) = desired {
                if self.pending_artwork.is_none() {
                    self.pending_artwork = Some(artwork.pin()?);
                }
                let reference = self.prepare_thumbnail()?;
                display.SetThumbnail(&reference)?;
            } else {
                display.SetThumbnail(None::<&RandomAccessStreamReference>)?;
            }
        }
        display.Update()?;
        if !unchanged {
            self.artwork_pin = self.pending_artwork.take();
            // SMTC owns the published RAM stream reference after Update; all
            // asynchronous writer operations have already reached terminal/Close.
            self.store = None;
        }
        Ok(())
    }
    pub(super) fn close(&mut self) -> Result<()> {
        self.retire_callbacks()?;
        self.retire_store()?;
        self.controls.SetIsEnabled(false)?;
        let display = self.controls.DisplayUpdater()?;
        display.ClearAll()?;
        display.SetThumbnail(None::<&RandomAccessStreamReference>)?;
        display.Update()?;
        self.artwork_pin = None;
        self.pending_artwork = None;
        Ok(())
    }
}
