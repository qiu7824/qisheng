#![cfg(any(windows, target_os = "linux"))]

#[path = "support/library_fixture.rs"]
mod fixture;

use lx_gpui_player::{
    catalog::Catalog,
    domain::library::*,
    library::{
        actor::{LibraryActor, LibraryCompletion},
        authority::{AuthorityRegistry, RootAuthority},
        scan::ScanContext,
    },
};
use std::{
    thread,
    time::{Duration, Instant},
};

fn run(
    actor: &mut LibraryActor,
    root: &LibraryRoot,
    operation: &str,
    content_evidence: bool,
) -> Duration {
    let token = LibraryScanToken {
        root_id: root.root_id.clone(),
        grant_epoch: root.grant_epoch,
        generation: root.generation,
        operation_id: operation.into(),
    };
    let context = ScanContext::new(Duration::from_secs(120));
    let started = Instant::now();
    if content_evidence {
        actor
            .submit_scan_with_content_evidence(
                token,
                root.clone(),
                LibraryScanOptions::default(),
                context,
            )
            .unwrap();
    } else {
        actor
            .submit_scan(token, root.clone(), LibraryScanOptions::default(), context)
            .unwrap();
    }
    let deadline = started + Duration::from_secs(120);
    let mut prepared_count = 0;
    loop {
        match actor.try_recv() {
            Some(LibraryCompletion::Batch { prepared, ack, .. }) => {
                prepared_count += prepared.len();
                assert!(ack.complete(Ok(())));
            }
            Some(LibraryCompletion::Finished {
                outcome,
                discovery_complete,
                ..
            }) => {
                assert!(discovery_complete);
                assert_eq!(
                    outcome.state,
                    LibraryOperationState::Completed,
                    "{outcome:?}"
                );
                assert_eq!(prepared_count, 401);
                return started.elapsed();
            }
            Some(other) => panic!("Unexpected preparation benchmark result: {other:?}"),
            None => {
                assert!(Instant::now() < deadline);
                thread::sleep(Duration::from_millis(1));
            }
        }
    }
}

#[test]
fn actual_401_metadata_preparation_cold_and_warm_benchmark() {
    let temp = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
    fixture::corpus(temp.path(), 401);
    let selected = RootAuthority::open(
        temp.path(),
        true,
        &ScanContext::new(Duration::from_secs(120)),
    )
    .unwrap();
    let draft = selected.draft();
    let barrier = selected.retirement_barrier();
    drop(selected);
    assert!(barrier.wait(Duration::from_secs(3)));
    let root = LibraryRoot {
        root_id: "root:actor-benchmark".into(),
        path: draft.path,
        recursive: true,
        grant_epoch: 1,
        anchor: draft.anchor,
        path_policy: draft.path_policy,
        watch_enabled: true,
        generation: 1,
        last_complete_generation: 0,
    };
    let registry = AuthorityRegistry::default();
    let mut actor =
        LibraryActor::new_with_authority(Catalog::new().unwrap(), registry.clone()).unwrap();
    let cold = run(&mut actor, &root, "benchmark:cold-metadata", false);
    let cold_stats = actor.metadata_cache_stats();
    let warm = run(&mut actor, &root, "benchmark:warm-metadata", false);
    let warm_stats = actor.metadata_cache_stats();
    let warm_content = run(&mut actor, &root, "benchmark:warm-content", true);
    let content_stats = actor.metadata_cache_stats();
    println!(
        "actor401 cold_metadata_s={:.3} warm_metadata_s={:.3} warm_content_s={:.3} cold={cold_stats:?} warm={warm_stats:?} content={content_stats:?}",
        cold.as_secs_f64(),
        warm.as_secs_f64(),
        warm_content.as_secs_f64()
    );
    assert_eq!(cold_stats.hits, 0);
    assert_eq!(cold_stats.entries, 401);
    assert_eq!(warm_stats.hits, 401);
    assert_eq!(content_stats.hits, 802);
    assert!(content_stats.owned_bytes <= 8 * 1024 * 1024);
    assert!(actor.shutdown(Duration::from_secs(5)).unwrap());
    assert_eq!(registry.retirement_pending(), 0);
}
