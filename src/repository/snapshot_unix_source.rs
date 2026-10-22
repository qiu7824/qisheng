//! SQLite owns every database/WAL/SHM descriptor and its POSIX lock lifetime.
use super::*;
use std::{
    fs::Metadata,
    mem::ManuallyDrop,
    os::unix::{fs::MetadataExt, io::FromRawFd},
    path::{Path, PathBuf},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Identity {
    device: u64,
    inode: u64,
}
fn identity(metadata: &Metadata) -> Result<Identity> {
    ensure!(
        metadata.is_file() && metadata.nlink() == 1,
        "Snapshot source must be a unique regular file"
    );
    Ok(Identity {
        device: metadata.dev(),
        inode: metadata.ino(),
    })
}
fn fd(document: &serde_json::Value) -> Result<i32> {
    document
        .get("h")
        .and_then(serde_json::Value::as_i64)
        .and_then(|value| i32::try_from(value).ok())
        .filter(|value| *value >= 0)
        .context("SQLite native descriptor is unavailable")
}
fn metadata(descriptor: i32) -> Result<Metadata> {
    // Borrow only. Closing even a duplicated descriptor would release POSIX
    // locks belonging to other SQLite connections in this process.
    let file = ManuallyDrop::new(unsafe { std::fs::File::from_raw_fd(descriptor) });
    Ok(file.metadata()?)
}
fn diagnostic(connection: &Connection, opcode: i32) -> Result<serde_json::Value> {
    ensure!(
        unsafe { ffi::sqlite3_libversion_number() } == 3_053_002,
        "Unix source descriptor inspection requires the pinned SQLite FILESTAT schema"
    );
    let mut file = ptr::null_mut::<ffi::sqlite3_file>();
    let code = unsafe {
        ffi::sqlite3_file_control(
            connection.handle(),
            c"main".as_ptr(),
            opcode,
            (&mut file as *mut *mut ffi::sqlite3_file).cast(),
        )
    };
    ensure!(
        code == ffi::SQLITE_OK && !file.is_null(),
        "SQLite source accessor is unavailable"
    );
    let methods = unsafe { (*file).pMethods };
    ensure!(!methods.is_null(), "SQLite source accessor has no methods");
    let control =
        unsafe { (*methods).xFileControl }.context("SQLite source file-control is unavailable")?;
    let builder = unsafe { ffi::sqlite3_str_new(ptr::null_mut()) };
    ensure!(
        !builder.is_null(),
        "Cannot allocate SQLite descriptor diagnostics"
    );
    let code = unsafe { control(file, ffi::SQLITE_FCNTL_FILESTAT, builder.cast()) };
    let length = unsafe { ffi::sqlite3_str_length(builder) };
    let error = unsafe { ffi::sqlite3_str_errcode(builder) };
    let text = unsafe { ffi::sqlite3_str_finish(builder) };
    let result = (|| {
        ensure!(
            code == ffi::SQLITE_OK && error == ffi::SQLITE_OK && !text.is_null(),
            "SQLite source diagnostics require SQLITE_ENABLE_FILESTAT"
        );
        ensure!(
            (1..=65536).contains(&length),
            "SQLite source diagnostic limit exceeded"
        );
        Ok(serde_json::from_slice(
            unsafe { CStr::from_ptr(text) }.to_bytes(),
        )?)
    })();
    unsafe { ffi::sqlite3_free(text.cast()) };
    result
}

pub(super) struct UnixSource {
    directory: SnapshotDirectory,
    paths: [PathBuf; 3],
    expected: [Identity; 3],
    baseline_wal: u64,
    context: SnapshotContext,
}
impl UnixSource {
    pub(super) fn open(
        original: &Connection,
        path: &Path,
        context: SnapshotContext,
    ) -> Result<(Connection, Self)> {
        context.check()?;
        let path = std::fs::canonicalize(path)?;
        let parent = path.parent().context("Snapshot source directory missing")?;
        let directory = SnapshotDirectory::pin_profile(parent, 1)?;
        let filename = path
            .file_name()
            .context("Snapshot source filename missing")?
            .to_string_lossy();
        let paths = [
            path.clone(),
            parent.join(format!("{filename}-wal")),
            parent.join(format!("{filename}-shm")),
        ];
        let main = diagnostic(original, ffi::SQLITE_FCNTL_FILE_POINTER)?;
        let wal = diagnostic(original, ffi::SQLITE_FCNTL_JOURNAL_POINTER)?;
        let shm = main
            .get("shm")
            .context("SQLite source SHM descriptor missing")?;
        let observed = [
            metadata(fd(&main)?)?,
            metadata(fd(&wal)?)?,
            metadata(fd(shm)?)?,
        ];
        let state = Self {
            directory,
            paths,
            expected: [
                identity(&observed[0])?,
                identity(&observed[1])?,
                identity(&observed[2])?,
            ],
            baseline_wal: observed[1].len(),
            context,
        };
        state.verify_names()?;
        let connection = Connection::open_with_flags(
            &path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        let opened = diagnostic(&connection, ffi::SQLITE_FCNTL_FILE_POINTER)?;
        ensure!(
            identity(&metadata(fd(&opened)?)?)? == state.expected[0],
            "SQLite reader opened another database"
        );
        connection
            .execute_batch("PRAGMA query_only=ON; PRAGMA mmap_size=0; PRAGMA cache_size=-2048;")?;
        state.verify_names()?;
        Ok((connection, state))
    }
    fn verify_names(&self) -> Result<()> {
        self.context.check()?;
        self.directory.verify()?;
        for (path, expected) in self.paths.iter().zip(&self.expected) {
            let metadata = std::fs::symlink_metadata(path)?;
            ensure!(
                !metadata.file_type().is_symlink() && identity(&metadata)? == *expected,
                "SQLite source namespace changed actual object"
            );
        }
        self.directory.verify()
    }
    pub(super) fn check(&self, connection: &Connection) -> Result<()> {
        self.verify_names()?;
        let main = diagnostic(connection, ffi::SQLITE_FCNTL_FILE_POINTER)?;
        let wal = diagnostic(connection, ffi::SQLITE_FCNTL_JOURNAL_POINTER)?;
        let shm = main
            .get("shm")
            .context("SQLite reader has no native SHM owner")?;
        let observed = [
            metadata(fd(&main)?)?,
            metadata(fd(&wal)?)?,
            metadata(fd(shm)?)?,
        ];
        for (metadata, expected) in observed.iter().zip(&self.expected) {
            ensure!(
                identity(metadata)? == *expected,
                "SQLite reader changed its actual native source"
            );
        }
        super::super::snapshot_native::enforce_wal_growth(observed[1].len(), self.baseline_wal)?;
        self.verify_names()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    #[test]
    fn unix_reader_keeps_a_coherent_cut_and_closing_it_preserves_other_writer_locks() {
        let folder = tempfile::tempdir().unwrap();
        let path = folder.path().join("含 空格.sqlite3");
        let writer = Connection::open(&path).unwrap();
        writer
            .execute_batch(
                "PRAGMA journal_mode=WAL; CREATE TABLE items(value); INSERT INTO items VALUES(1);",
            )
            .unwrap();
        let mut reader = SourceSqlite::open_unix(
            &writer,
            &path,
            SnapshotContext::new(Duration::from_secs(20)).unwrap(),
        )
        .unwrap();
        reader
            .connection()
            .unwrap()
            .execute_batch("BEGIN DEFERRED")
            .unwrap();
        let count = |sql: &Connection| {
            sql.query_row("SELECT count(*) FROM items", [], |r| r.get::<_, i64>(0))
                .unwrap()
        };
        assert_eq!(count(reader.connection().unwrap()), 1);
        writer.execute_batch("INSERT INTO items VALUES(2)").unwrap();
        reader.check().unwrap();
        assert_eq!(count(reader.connection().unwrap()), 1);
        writer
            .execute_batch("BEGIN IMMEDIATE; INSERT INTO items VALUES(3)")
            .unwrap();
        reader
            .connection()
            .unwrap()
            .execute_batch("ROLLBACK")
            .unwrap();
        reader.close().unwrap();
        drop(reader);
        let rival = Connection::open(&path).unwrap();
        rival.busy_timeout(Duration::ZERO).unwrap();
        assert!(
            rival.execute_batch("BEGIN IMMEDIATE").is_err(),
            "Closing the reader released another connection's writer lock"
        );
        writer.execute_batch("COMMIT").unwrap();
        rival.execute_batch("BEGIN IMMEDIATE; ROLLBACK").unwrap();
        assert_eq!(count(&rival), 3);
    }
}
