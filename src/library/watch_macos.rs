//! FSEvents notifications remain hints; native descriptors retain root authority.
use super::*;
use crate::library::authority::{AuthorizedDirectory, RootAuthority};
use anyhow::Context as _;
use notify::event::{CreateKind, ModifyKind, RemoveKind, RenameMode};
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use std::os::unix::{ffi::OsStrExt, fs::MetadataExt, io::AsRawFd};
use std::time::Instant;

type Key = (String, u64);
enum Delivery {
    Event(Key, Event),
    Error(Key, String),
}
struct Context {
    // Dropping FSEvents stops and joins its native owner before descriptor release.
    watcher: RecommendedWatcher,
    directory: AuthorizedDirectory,
    authority: RootAuthority,
    root: LibraryRoot,
    token: LibraryWatchToken,
    dirty: Arc<DirtyState>,
    overflow: Arc<AtomicBool>,
    _slot: Credit,
    active: bool,
    path: PathBuf,
}

fn physical_path(context: &Context) -> Result<PathBuf> {
    context.authority.check()?;
    let file = context.directory.native_file()?;
    let metadata = file.metadata()?;
    ensure!(
        format!("{:016x}", metadata.dev()) == context.root.anchor.volume
            && format!("{:016x}", metadata.ino()) == context.root.anchor.file_id,
        "NeedsReapproval: FSEvents root identity changed"
    );
    let mut bytes = [0u8; libc::PATH_MAX as usize];
    ensure!(
        unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETPATH, bytes.as_mut_ptr()) } >= 0,
        "Cannot resolve watched macOS root: {}",
        std::io::Error::last_os_error()
    );
    let end = bytes
        .iter()
        .position(|b| *b == 0)
        .context("Unterminated FSEvents root path")?;
    Ok(PathBuf::from(std::ffi::OsStr::from_bytes(&bytes[..end])))
}

fn mode(recursive: bool) -> RecursiveMode {
    if recursive {
        RecursiveMode::Recursive
    } else {
        RecursiveMode::NonRecursive
    }
}

fn prepare(
    authority: &AuthorityRegistry,
    root: LibraryRoot,
    token: LibraryWatchToken,
    dirty: Arc<DirtyState>,
    slot: Credit,
    sender: SyncSender<Delivery>,
) -> Result<Context> {
    let verified = authority.watch_authority(&root)?;
    let directory = verified.open_directory(&root.path)?;
    let overflow = Arc::new(AtomicBool::new(false));
    let overflow_callback = overflow.clone();
    let key = (token.root_id.clone(), token.watch_generation);
    let wake = std::thread::current();
    let mut watcher = notify::recommended_watcher(move |result: notify::Result<Event>| {
        let delivery = match result {
            Ok(event) => {
                if event.paths.len() > MAX_HINTS
                    || event
                        .paths
                        .iter()
                        .map(|p| p.as_os_str().len())
                        .sum::<usize>()
                        > MAX_HINT_BYTES
                {
                    overflow_callback.store(true, Ordering::Release);
                    wake.unpark();
                    return;
                }
                Delivery::Event(key.clone(), event)
            }
            Err(error) => {
                Delivery::Error(key.clone(), error.to_string().chars().take(1024).collect())
            }
        };
        if sender.try_send(delivery).is_err() {
            overflow_callback.store(true, Ordering::Release);
        }
        wake.unpark();
    })?;
    watcher.watch(&root.path, mode(root.recursive))?;
    directory.check()?;
    Ok(Context {
        path: root.path.clone(),
        watcher,
        directory,
        authority: verified,
        root,
        token,
        dirty,
        overflow,
        _slot: slot,
        active: false,
    })
}

fn emit(context: &Context, event: Event, output: &mut Output) {
    let mut full = event.need_rescan() || matches!(event.kind, EventKind::Any | EventKind::Other);
    let is_directory = match event.kind {
        EventKind::Create(CreateKind::Folder) | EventKind::Remove(RemoveKind::Folder) => Some(true),
        EventKind::Create(CreateKind::File) | EventKind::Remove(RemoveKind::File) => Some(false),
        _ => None,
    };
    full |= is_directory == Some(true);
    let mut hints = Vec::new();
    for (index, path) in event.paths.iter().enumerate() {
        let Ok(relative) = path.strip_prefix(&context.path) else {
            full = true;
            continue;
        };
        if relative.as_os_str().is_empty() {
            full = true;
            continue;
        }
        if !safe_relative(relative) {
            full = true;
            continue;
        }
        if !context.root.recursive && relative.components().count() != 1 {
            continue;
        }
        let kind = match event.kind {
            EventKind::Create(_) => NativeWatchKind::Created,
            EventKind::Remove(_) => NativeWatchKind::Removed,
            EventKind::Modify(ModifyKind::Name(RenameMode::From)) => NativeWatchKind::RenamedOld,
            EventKind::Modify(ModifyKind::Name(RenameMode::To)) => NativeWatchKind::RenamedNew,
            EventKind::Modify(ModifyKind::Name(RenameMode::Both)) => {
                if index == 0 {
                    NativeWatchKind::RenamedOld
                } else {
                    NativeWatchKind::RenamedNew
                }
            }
            EventKind::Modify(ModifyKind::Name(_)) => {
                full = true;
                NativeWatchKind::Modified
            }
            EventKind::Access(_) => continue,
            _ => NativeWatchKind::Modified,
        };
        hints.push(NativeWatchHint {
            path: relative.to_path_buf(),
            kind,
            is_directory,
            rename_cookie: None,
        });
    }
    if full || !hints.is_empty() {
        output.dirty(&context.dirty, hints, full, context.active);
    }
}

pub(super) fn run(
    authority: AuthorityRegistry,
    commands: Receiver<Command>,
    shared: Arc<Shared>,
    output: &mut Output,
) -> Result<()> {
    let (sender, events) = mpsc::sync_channel(EVENT_CAPACITY);
    let mut contexts = BTreeMap::<Key, Context>::new();
    let mut probe_at = Instant::now();
    while !shared.closing.load(Ordering::Acquire) {
        output.flush();
        for _ in 0..CONTROL_CAPACITY {
            let Ok(command) = commands.try_recv() else {
                break;
            };
            match command {
                Command::Prepare {
                    root,
                    token,
                    dirty,
                    slot,
                    credit,
                } => {
                    let key = (token.root_id.clone(), token.watch_generation);
                    let already_owned = contexts.contains_key(&key);
                    let result = if already_owned {
                        Err(anyhow::anyhow!("FSEvents generation already has an owner"))
                    } else {
                        prepare(&authority, root, token.clone(), dirty, slot, sender.clone()).map(
                            |context| {
                                contexts.insert(key, context);
                            },
                        )
                    };
                    if result.is_err() && !already_owned {
                        output.forget(&token.root_id, token.watch_generation);
                    }
                    output.control(NativeWatchEvent::Prepared { token, result }, Some(credit));
                }
                Command::Activate { token, credit } => {
                    if let Some(context) =
                        contexts.get_mut(&(token.root_id.clone(), token.watch_generation))
                    {
                        context.active = true;
                        output.control(NativeWatchEvent::Watching { token }, Some(credit));
                        output.baseline(&context.dirty);
                    } else {
                        output.control(
                            NativeWatchEvent::Error {
                                token,
                                error: LibraryError::conflict(
                                    "Prepared FSEvents generation is no longer owned",
                                ),
                            },
                            Some(credit),
                        );
                    }
                }
                Command::Stop {
                    root_id,
                    generation,
                    credit,
                } => {
                    drop(contexts.remove(&(root_id.clone(), generation)));
                    output.forget(&root_id, generation);
                    output.control(
                        NativeWatchEvent::Stopped {
                            root_id,
                            watch_generation: generation,
                        },
                        Some(credit),
                    );
                }
            }
        }
        for _ in 0..EVENT_CAPACITY {
            match events.try_recv() {
                Ok(Delivery::Event(key, event)) => {
                    if let Some(context) = contexts.get(&key) {
                        emit(context, event, output);
                    }
                }
                Ok(Delivery::Error(key, message)) => {
                    if let Some(context) = contexts.get(&key) {
                        output.dirty(&context.dirty, Vec::new(), true, context.active);
                        output.control(
                            NativeWatchEvent::Error {
                                token: context.token.clone(),
                                error: LibraryError::new(LibraryErrorKind::Unavailable, message),
                            },
                            None,
                        );
                    }
                }
                Err(_) => break,
            }
        }
        for context in contexts.values() {
            if context.overflow.swap(false, Ordering::AcqRel) {
                output.dirty(&context.dirty, Vec::new(), true, context.active);
            }
        }
        if probe_at.elapsed() >= Duration::from_millis(250) {
            probe_at = Instant::now();
            let mut retired = Vec::new();
            for (key, context) in &mut contexts {
                match physical_path(context) {
                    Ok(path) if path != context.path => {
                        output.dirty(&context.dirty, Vec::new(), true, context.active);
                        output.control(
                            NativeWatchEvent::RootMoved {
                                token: context.token.clone(),
                                current_path: path.clone(),
                                anchor: context.root.anchor.clone(),
                            },
                            None,
                        );
                        if context.token.options.root_move_policy
                            == crate::domain::library_watch::RootMovePolicy::Pause
                        {
                            context.dirty.fail(&LibraryError::new(
                                LibraryErrorKind::Unavailable,
                                "Native watch root moved",
                            ));
                            retired.push(key.clone());
                        } else {
                            let rebound = context.watcher.unwatch(&context.path).and_then(|()| {
                                context.watcher.watch(&path, mode(context.root.recursive))
                            });
                            if let Err(error) = rebound {
                                output.control(
                                    NativeWatchEvent::Error {
                                        token: context.token.clone(),
                                        error: LibraryError::new(
                                            LibraryErrorKind::Unavailable,
                                            error.to_string(),
                                        ),
                                    },
                                    None,
                                );
                                retired.push(key.clone());
                            } else {
                                context.path = path;
                            }
                        }
                    }
                    Err(error) => {
                        output.dirty(&context.dirty, Vec::new(), true, context.active);
                        output.control(
                            NativeWatchEvent::Error {
                                token: context.token.clone(),
                                error: watch_error(&error),
                            },
                            None,
                        );
                        retired.push(key.clone());
                    }
                    _ => {}
                }
            }
            for key in retired {
                drop(contexts.remove(&key));
            }
        }
        std::thread::park_timeout(Duration::from_millis(25));
    }
    // FSEvents owners join before the managed worker reports completion.
    drop(contexts);
    while let Ok(command) = commands.try_recv() {
        if let Command::Prepare { token, .. } = command {
            output.forget(&token.root_id, token.watch_generation);
        }
    }
    shared
        .dirty
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clear();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::scan::ScanContext;

    fn start(path: &Path) -> (AuthorityRegistry, NativeWatch, LibraryWatchToken) {
        let registry = AuthorityRegistry::default();
        let opened =
            RootAuthority::open(path, true, &ScanContext::new(Duration::from_secs(10))).unwrap();
        let draft = opened.draft();
        let root = LibraryRoot {
            root_id: "root:mac-watch".into(),
            path: draft.path,
            recursive: true,
            grant_epoch: 1,
            anchor: draft.anchor,
            path_policy: draft.path_policy,
            watch_enabled: false,
            generation: 0,
            last_complete_generation: 0,
        };
        registry
            .accept_root_proof(&root, opened.proof().unwrap())
            .unwrap();
        let token = LibraryWatchToken {
            root_id: root.root_id.clone(),
            grant_epoch: 1,
            watch_generation: 1,
            enabled: true,
            options: Default::default(),
        };
        let mut watch = NativeWatch::new(registry.clone()).unwrap();
        watch.prepare(root, token.clone(), 0).unwrap();
        match next(&mut watch) {
            NativeWatchEvent::Prepared { result, .. } => result.unwrap(),
            event => panic!("{event:?}"),
        }
        watch.activate(token.clone()).unwrap();
        assert!(matches!(
            next(&mut watch),
            NativeWatchEvent::Watching { .. }
        ));
        assert!(matches!(next(&mut watch), NativeWatchEvent::Dirty { .. }));
        (registry, watch, token)
    }
    fn next(watch: &mut NativeWatch) -> NativeWatchEvent {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(event) = watch.try_recv() {
                return event;
            }
            assert!(
                Instant::now() < deadline,
                "Native FSEvents response did not arrive"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    #[test]
    fn actual_create_is_observed_and_stop_joins_the_native_owner() {
        let folder = tempfile::tempdir().unwrap();
        let (registry, mut watch, token) = start(folder.path());
        let baseline = watch.dirty(&token.root_id, 1).unwrap().received_sequence;
        watch.cover(&token, baseline, true).unwrap();
        std::fs::write(folder.path().join("歌曲.wav"), b"watch fixture").unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            while watch.try_recv().is_some() {}
            if watch.dirty(&token.root_id, 1).unwrap().received_sequence > baseline {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "FSEvents did not report an actual filesystem write"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        watch.stop(&token.root_id, 1).unwrap();
        loop {
            if matches!(next(&mut watch), NativeWatchEvent::Stopped { .. }) {
                break;
            }
        }
        assert_eq!(watch.shared.context_count.load(Ordering::Acquire), 0);
        assert!(watch.shutdown(Duration::from_secs(5)).unwrap());
        registry.begin_shutdown();
        assert!(registry.retirement_barrier().wait(Duration::from_secs(5)));
    }
    #[test]
    fn revoked_root_retires_fsevents_and_marks_a_rescan() {
        let folder = tempfile::tempdir().unwrap();
        let (registry, mut watch, token) = start(folder.path());
        registry.revoke_root(&token.root_id);
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            while watch.try_recv().is_some() {}
            if watch.shared.context_count.load(Ordering::Acquire) == 0 {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "Revoked native watch remained live"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        let dirty = watch.dirty(&token.root_id, 1).unwrap();
        assert!(dirty.full_rescan_required && dirty.error.is_some());
        assert!(watch.shutdown(Duration::from_secs(5)).unwrap());
    }
    #[test]
    fn moved_root_reports_its_same_object_location_and_pauses() {
        let folder = tempfile::tempdir().unwrap();
        let original = folder.path().join("original");
        let renamed = folder.path().join("renamed");
        std::fs::create_dir(&original).unwrap();
        let (_registry, mut watch, token) = start(&original);
        std::fs::rename(&original, &renamed).unwrap();
        loop {
            if let NativeWatchEvent::RootMoved { current_path, .. } = next(&mut watch) {
                assert_eq!(current_path, std::fs::canonicalize(&renamed).unwrap());
                break;
            }
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        while watch.shared.context_count.load(Ordering::Acquire) != 0 {
            assert!(
                Instant::now() < deadline,
                "Moved root retained an active native watcher"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        let dirty = watch.dirty(&token.root_id, 1).unwrap();
        assert!(dirty.full_rescan_required && dirty.error.is_some());
        assert!(watch.shutdown(Duration::from_secs(5)).unwrap());
    }
}
