use lx_gpui_player::{source::MAX_SCRIPT_BYTES, worker::SourceWorker};

#[test]
fn selected_file_is_validated_in_the_worker_without_execution_or_approval() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("源文件 with spaces.js");
    let script = "/* @name Static selected file */\nthrow new Error('must never execute during file validation');";
    std::fs::write(&path, script).unwrap();
    let mut worker = SourceWorker::prepare().unwrap();
    let imported = worker.validate_file(&path).unwrap();
    assert_eq!(imported.script, script);
    assert!(!imported.approved);
    imported.verify_integrity().unwrap();
}

#[test]
fn invalid_file_bytes_and_oversized_input_are_rejected_and_validation_recovers() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("source.js");
    let mut worker = SourceWorker::prepare().unwrap();
    std::fs::write(&path, [0xff, 0xfe]).unwrap();
    assert!(
        worker
            .validate_file(&path)
            .unwrap_err()
            .to_string()
            .contains("UTF-8")
    );
    std::fs::write(&path, vec![b'x'; MAX_SCRIPT_BYTES + 1]).unwrap();
    assert!(
        worker
            .validate_file(&path)
            .unwrap_err()
            .to_string()
            .contains("1 MiB")
    );
    std::fs::write(&path, "/* @name Recovery file */\nconst untouched = true;").unwrap();
    assert!(!worker.validate_file(&path).unwrap().approved);
    assert!(worker.validate_file(directory.path()).is_err());
}
