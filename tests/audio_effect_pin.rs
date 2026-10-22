use lx_gpui_player::audio::effect_pin::{EffectPinRender, PinScope, PinStatus, try_new};
use std::{
    alloc::{GlobalAlloc, Layout, System},
    cell::Cell,
    sync::{Arc, Barrier, mpsc},
    thread,
};

thread_local! {
    static WATCH: Cell<bool> = const { Cell::new(false) };
    static HEAP: Cell<(usize, usize)> = const { Cell::new((0, 0)) };
}
struct Allocator;
unsafe impl GlobalAlloc for Allocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let _ = WATCH.try_with(|watch| {
            if watch.get() {
                HEAP.with(|heap| {
                    let (allocated, freed) = heap.get();
                    heap.set((allocated + 1, freed));
                });
            }
        });
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        let _ = WATCH.try_with(|watch| {
            if watch.get() {
                HEAP.with(|heap| {
                    let (allocated, freed) = heap.get();
                    heap.set((allocated, freed + 1));
                });
            }
        });
        unsafe { System.dealloc(pointer, layout) }
    }
}
#[global_allocator]
static ALLOCATOR: Allocator = Allocator;

fn no_heap<T>(operation: impl FnOnce() -> T) -> T {
    struct Guard;
    impl Drop for Guard {
        fn drop(&mut self) {
            WATCH.with(|watch| watch.set(false));
        }
    }
    HEAP.with(|heap| heap.set((0, 0)));
    WATCH.with(|watch| watch.set(true));
    let guard = Guard;
    let value = operation();
    drop(guard);
    assert_eq!(
        HEAP.with(Cell::get),
        (0, 0),
        "Pin callback allocated or destroyed heap storage"
    );
    value
}
fn scope() -> PinScope {
    PinScope {
        bank_owner: 19,
        media_token: 37,
        source_epoch: 7,
        sample_rate: 48000,
        device_channels: 2,
    }
}
fn idle() -> PinScope {
    PinScope {
        bank_owner: 0,
        media_token: 0,
        source_epoch: 0,
        sample_rate: 22050,
        device_channels: 1,
    }
}

#[test]
fn cancel_and_ticket_drop_never_release_admission_before_callback_acknowledgement() {
    let (owner, mut render) = try_new().unwrap();
    let first = owner.request(scope()).unwrap();
    let identity = first.identity();
    assert_eq!(first.status(), PinStatus::Requested);
    assert!(no_heap(|| render.poll(scope())));
    assert!(first.is_pinned_for(scope()));
    first.cancel();
    assert_eq!(first.status(), PinStatus::CancelRequested);
    assert!(!first.is_pinned_for(scope()));
    assert!(owner.request(scope()).is_err());
    assert_eq!(owner.current_ticket_identity(), Some(identity));
    assert!(owner.reap().is_none());
    assert!(!no_heap(|| render.poll(scope())));
    assert_eq!(first.status(), PinStatus::Released);
    assert_eq!(owner.reap(), Some(identity));
    let next = owner.request(idle()).unwrap();
    assert!(next.identity().serial > identity.serial);
    first.cancel();
    drop(first);
    assert!(!next.cancel_requested());
    assert!(no_heap(|| render.poll(idle())));
    assert!(next.is_pinned_for(idle()));
    let next_identity = next.identity();
    drop(next);
    assert!(owner.request(idle()).is_err());
    assert!(owner.reap().is_none());
    assert!(!no_heap(|| render.poll(idle())));
    assert_eq!(owner.reap(), Some(next_identity));
    owner.close_on_owner();
}

#[test]
fn every_live_scope_component_fences_the_pin_and_a_rejected_request_cannot_resurrect() {
    let original = scope();
    let mismatches = [
        PinScope {
            bank_owner: 0,
            ..original
        },
        PinScope {
            bank_owner: 20,
            ..original
        },
        PinScope {
            media_token: 38,
            ..original
        },
        PinScope {
            source_epoch: 8,
            ..original
        },
        PinScope {
            sample_rate: 44100,
            ..original
        },
        PinScope {
            device_channels: 1,
            ..original
        },
    ];
    let (owner, mut render) = try_new().unwrap();
    for actual in mismatches {
        let ticket = owner.request(original).unwrap();
        assert!(no_heap(|| render.poll(original)));
        assert!(!ticket.is_pinned_for(actual));
        assert!(!no_heap(|| render.poll(actual)));
        assert_eq!(ticket.status(), PinStatus::Rejected);
        for _ in 0..8 {
            assert!(!no_heap(|| render.poll(original)));
            assert_eq!(ticket.status(), PinStatus::Rejected);
        }
        assert_eq!(owner.reap(), Some(ticket.identity()));
    }
    owner.close_on_owner();
}

#[test]
fn idle_no_effects_pin_rejects_an_incoming_bank_and_close_is_terminal() {
    let (owner, mut render) = try_new().unwrap();
    let ticket = owner.request(idle()).unwrap();
    assert!(no_heap(|| render.poll(idle())));
    assert!(ticket.is_pinned_for(idle()));
    assert!(!no_heap(|| render.poll(scope())));
    assert_eq!(ticket.status(), PinStatus::Rejected);
    let current = owner.request(scope()).unwrap();
    assert!(no_heap(|| render.poll(scope())));
    assert!(current.is_pinned_for(scope()));
    // The callback handle is handed back before terminal owner closure.
    let returned_render = render;
    owner.close_on_owner();
    assert_eq!(current.status(), PinStatus::Closed);
    assert!(!current.is_pinned_for(scope()));
    assert!(owner.current_ticket_identity().is_none());
    assert!(owner.request(idle()).is_err());
    assert!(owner.clone().request(scope()).is_err());
    drop(returned_render);
}

#[test]
fn cancellation_before_first_poll_is_acknowledged_without_temporarily_pinning() {
    let (owner, mut render) = try_new().unwrap();
    let ticket = owner.request(scope()).unwrap();
    ticket.cancel();
    assert!(owner.request(idle()).is_err());
    assert!(!no_heap(|| render.poll(scope())));
    assert_eq!(ticket.status(), PinStatus::Released);
    let next = owner.request(idle()).unwrap();
    assert!(no_heap(|| render.poll(idle())));
    assert!(next.is_pinned_for(idle()));
    owner.close_on_owner();
}

#[test]
fn concurrent_cancellation_and_retry_preserve_request_scope_and_real_release_order() {
    let (owner, render) = try_new().unwrap();
    let ticket = Arc::new(owner.request(scope()).unwrap());
    let barrier = Arc::new(Barrier::new(3));
    let (poll_tx, poll_rx) = mpsc::sync_channel::<PinScope>(1);
    let (reply_tx, reply_rx) = mpsc::sync_channel(1);
    let callback = thread::spawn(move || {
        let mut render = render;
        while let Ok(actual) = poll_rx.recv() {
            let freeze = no_heap(|| render.poll(actual));
            reply_tx.send(freeze).unwrap();
        }
        render
    });
    poll_tx.send(scope()).unwrap();
    assert!(reply_rx.recv().unwrap());
    let cancel_ticket = ticket.clone();
    let cancel_barrier = barrier.clone();
    let cancelling = thread::spawn(move || {
        cancel_barrier.wait();
        cancel_ticket.cancel();
    });
    let retry_owner = owner.clone();
    let retry_barrier = barrier.clone();
    let retrying = thread::spawn(move || {
        retry_barrier.wait();
        // Cancellation and retry race while callback release is deliberately
        // withheld; neither ordering permits another active operation.
        retry_owner.request(idle()).is_err()
    });
    barrier.wait();
    cancelling.join().unwrap();
    assert!(retrying.join().unwrap());
    assert_eq!(ticket.status(), PinStatus::CancelRequested);
    assert!(owner.clone().request(idle()).is_err());
    poll_tx.send(scope()).unwrap();
    assert!(!reply_rx.recv().unwrap());
    assert_eq!(ticket.status(), PinStatus::Released);
    let mut previous = ticket.identity().serial;
    // Differing complete payloads cross the owner/callback thread boundary.
    // Holding the oldest ticket exercises late cancellation across every retry.
    for index in 1..=256 {
        let actual = PinScope {
            bank_owner: index,
            media_token: index + 1000,
            source_epoch: index + 2000,
            sample_rate: if index % 2 == 0 { 44100 } else { 48000 },
            device_channels: if index % 3 == 0 { 1 } else { 2 },
        };
        let next = owner.request(actual).unwrap();
        assert!(next.identity().serial > previous);
        previous = next.identity().serial;
        ticket.cancel();
        poll_tx.send(actual).unwrap();
        assert!(reply_rx.recv().unwrap());
        assert!(next.is_pinned_for(actual));
        assert!(!next.is_pinned_for(scope()));
        next.cancel();
        assert!(owner.request(scope()).is_err());
        poll_tx.send(actual).unwrap();
        assert!(!reply_rx.recv().unwrap());
        assert_eq!(next.status(), PinStatus::Released);
    }
    drop(poll_tx);
    let returned_render: EffectPinRender = callback.join().unwrap();
    owner.close_on_owner();
    drop(returned_render);
}

#[test]
fn invalid_scope_never_changes_the_active_request_or_its_pin() {
    let (owner, mut render) = try_new().unwrap();
    let ticket = owner.request(scope()).unwrap();
    assert!(no_heap(|| render.poll(scope())));
    let invalid = [
        PinScope {
            sample_rate: 0,
            ..scope()
        },
        PinScope {
            device_channels: 0,
            ..scope()
        },
        PinScope {
            device_channels: 9,
            ..scope()
        },
        PinScope {
            media_token: 0,
            ..scope()
        },
        PinScope {
            source_epoch: 0,
            ..scope()
        },
        PinScope {
            bank_owner: 1,
            ..idle()
        },
    ];
    for bad in invalid {
        assert!(owner.request(bad).is_err());
        assert_eq!(owner.current_ticket_identity(), Some(ticket.identity()));
        assert!(no_heap(|| render.poll(scope())));
        assert!(ticket.is_pinned_for(scope()));
    }
    owner.close_on_owner();
}
