use lx_gpui_player::library::scan::*;
use std::{fs, sync::atomic::Ordering, time::Duration};
fn root() -> tempfile::TempDir {
    tempfile::tempdir_in(std::env::temp_dir()).unwrap()
}
fn context() -> ScanContext {
    ScanContext::new(Duration::from_secs(3))
}
#[test]
fn explicit_discovery_is_deterministic_recursive_and_does_not_read_or_modify_audio() {
    let directory = root();
    fs::create_dir(directory.path().join("nested")).unwrap();
    for name in [
        "b.MP3",
        "a.wav",
        "not.txt",
        "nested/c.flac",
        "nested/lyric.lrc",
    ] {
        fs::write(directory.path().join(name), [1, 2, 3]).unwrap();
    }
    let request = ScanRequest::new(directory.path().to_path_buf());
    let first = discover(&request, &context()).unwrap();
    let second = discover(&request, &context()).unwrap();
    assert!(first.complete);
    assert_eq!(first.files, second.files);
    assert_eq!(first.files.len(), 3);
    assert_eq!(first.directories_visited, 2);
    assert_eq!(first.entries_examined, 6);
    assert!(
        first
            .files
            .iter()
            .all(|file| file.size_bytes == 3 && file.path.is_absolute())
    );
    assert_eq!(fs::read(directory.path().join("a.wav")).unwrap(), [1, 2, 3]);
}
#[test]
fn non_recursive_scan_and_depth_limit_have_explicit_completeness() {
    let directory = root();
    fs::create_dir(directory.path().join("nested")).unwrap();
    fs::write(directory.path().join("root.wav"), [1]).unwrap();
    fs::write(directory.path().join("nested/child.wav"), [2]).unwrap();
    let mut request = ScanRequest::new(directory.path().to_path_buf());
    request.recursive = false;
    let report = discover(&request, &context()).unwrap();
    assert!(report.complete);
    assert_eq!(report.files.len(), 1);
    request.recursive = true;
    request.max_depth = 0;
    let report = discover(&request, &context()).unwrap();
    assert!(!report.complete);
    assert_eq!(report.files.len(), 1);
    assert_eq!(report.warnings.len(), 1);
}
#[test]
fn file_and_entry_budgets_fail_without_claiming_a_complete_partial_result() {
    let directory = root();
    for name in ["a.wav", "b.wav", "c.wav"] {
        fs::write(directory.path().join(name), [1]).unwrap();
    }
    let mut request = ScanRequest::new(directory.path().to_path_buf());
    request.max_files = 2;
    assert_eq!(
        discover(&request, &context())
            .unwrap_err()
            .downcast_ref::<ScanError>(),
        Some(&ScanError::BudgetExceeded)
    );
    request.max_files = 3;
    request.max_entries = 2;
    assert_eq!(
        discover(&request, &context())
            .unwrap_err()
            .downcast_ref::<ScanError>(),
        Some(&ScanError::BudgetExceeded)
    );
}
#[test]
fn cancelled_expired_relative_and_non_directory_requests_are_rejected() {
    let directory = root();
    let request = ScanRequest::new(directory.path().to_path_buf());
    let cancelled = context();
    cancelled.cancel.store(true, Ordering::Release);
    assert_eq!(
        discover(&request, &cancelled)
            .unwrap_err()
            .downcast_ref::<ScanError>(),
        Some(&ScanError::Cancelled)
    );
    assert!(discover(&request, &ScanContext::new(Duration::MAX)).is_err());
    assert_eq!(
        discover(&request, &ScanContext::new(Duration::ZERO))
            .unwrap_err()
            .downcast_ref::<ScanError>(),
        Some(&ScanError::DeadlineExceeded)
    );
    assert!(discover(&ScanRequest::new("relative".into()), &context()).is_err());
    fs::write(directory.path().join("file"), [0]).unwrap();
    assert!(discover(&ScanRequest::new(directory.path().join("file")), &context()).is_err());
}
#[cfg(unix)]
#[test]
fn nested_symlink_cannot_expand_the_selected_directory_grant() {
    use std::os::unix::fs::symlink;
    let directory = root();
    let outside = root();
    fs::write(outside.path().join("outside.wav"), [1]).unwrap();
    symlink(outside.path(), directory.path().join("link")).unwrap();
    let report = discover(
        &ScanRequest::new(directory.path().to_path_buf()),
        &context(),
    )
    .unwrap();
    assert!(report.files.is_empty());
    assert_eq!(report.warnings.len(), 1);
}

#[cfg(unix)]
#[test]
fn non_utf8_audio_paths_are_rejected_before_a_json_success_report() {
    use std::os::unix::ffi::OsStringExt;
    let directory = root();
    let name = std::ffi::OsString::from_vec(b"non-utf8-\xff.wav".to_vec());
    let invalid_path = directory.path().join(name);
    let selected = match fs::write(&invalid_path, [0]) {
        Ok(()) => directory.path().to_path_buf(),
        #[cfg(target_os = "macos")]
        Err(error) if error.raw_os_error() == Some(libc::EILSEQ) => {
            // APFS rejects the directory entry itself; the public request must
            // still reject this non-Unicode path before attempting filesystem I/O.
            invalid_path
        }
        Err(error) => panic!("Cannot create the invalid-name fixture: {error}"),
    };
    let error = discover(&ScanRequest::new(selected), &context()).unwrap_err();
    assert_eq!(
        error.downcast_ref::<ScanError>(),
        Some(&ScanError::UnsupportedPath)
    );
}
