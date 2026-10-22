//! A profile owns one inotify instance. A wd may serve several verified roots.
use super::*;
use crate::library::authority::{AuthorizedDirectory, RootAuthority};
use anyhow::Context as _;
use std::collections::BTreeSet;
use std::ffi::{CString, OsStr};
use std::fs::File;
use std::io;
use std::os::unix::{
    ffi::OsStrExt,
    fs::MetadataExt,
    io::{AsRawFd, FromRawFd},
};
use std::time::Instant;

const MAX_WDS: usize = 10_000;
const MAX_SUBSCRIPTIONS: usize = 20_000;
const MAX_DIRECTORY_BYTES: usize = 8 * 1024 * 1024;
const MASK: u32 = libc::IN_CREATE
    | libc::IN_MODIFY
    | libc::IN_ATTRIB
    | libc::IN_CLOSE_WRITE
    | libc::IN_DELETE
    | libc::IN_MOVED_FROM
    | libc::IN_MOVED_TO
    | libc::IN_DELETE_SELF
    | libc::IN_MOVE_SELF
    | libc::IN_ONLYDIR;
struct Context {
    root: LibraryRoot,
    token: LibraryWatchToken,
    authority: RootAuthority,
    directory: AuthorizedDirectory,
    dirty: Arc<DirtyState>,
    _slot: Credit,
    active: bool,
    retiring: bool,
    pending_ignored: usize,
    stops: Vec<Credit>,
    wds: BTreeSet<i32>,
    refresh: bool,
    last_path: PathBuf,
}
struct Node {
    _file: File,
    dev: u64,
    ino: u64,
    subscriptions: BTreeMap<usize, PathBuf>,
    waiters: BTreeSet<usize>,
    retiring: bool,
}
struct Owner {
    inotify: File,
    wake: File,
    nodes: BTreeMap<i32, Node>,
    contexts: Vec<Option<Context>>,
    subscriptions: usize,
    subscription_bytes: usize,
    rebuild_requested: bool,
    shared: Arc<Shared>,
}
impl Owner {
    fn new(shared: Arc<Shared>) -> Result<Self> {
        let raw = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
        ensure!(
            raw >= 0,
            "Cannot create inotify owner: {}",
            io::Error::last_os_error()
        );
        let inotify = unsafe { File::from_raw_fd(raw) };
        let raw = unsafe { libc::eventfd(0, libc::EFD_NONBLOCK | libc::EFD_CLOEXEC) };
        ensure!(
            raw >= 0,
            "Cannot create inotify wake descriptor: {}",
            io::Error::last_os_error()
        );
        let wake = unsafe { File::from_raw_fd(raw) };
        shared.wake_fd.store(wake.as_raw_fd(), Ordering::Release);
        Ok(Self {
            inotify,
            wake,
            nodes: BTreeMap::new(),
            contexts: (0..MAX_CONTEXTS).map(|_| None).collect(),
            subscriptions: 0,
            subscription_bytes: 0,
            rebuild_requested: false,
            shared,
        })
    }
    fn add(
        &mut self,
        index: usize,
        directory: AuthorizedDirectory,
        relative: PathBuf,
    ) -> Result<()> {
        ensure!(
            !self.nodes.values().any(|n| n.retiring),
            LibraryError::new(
                LibraryErrorKind::Busy,
                "inotify removal is awaiting IN_IGNORED"
            )
        );
        ensure!(
            relative.as_os_str().is_empty() || safe_relative(&relative),
            "Invalid watched directory locator"
        );
        let file = directory.native_file()?;
        let metadata = file.metadata()?;
        let dev = metadata.dev();
        let ino = metadata.ino();
        let existing = self
            .nodes
            .iter()
            .find(|(_, n)| !n.retiring && n.dev == dev && n.ino == ino)
            .map(|(wd, _)| *wd);
        if let Some(wd) = existing {
            let node = self.nodes.get_mut(&wd).unwrap();
            if let Some(previous) = node.subscriptions.get(&index) {
                let next_bytes =
                    self.subscription_bytes - directory_cost(previous) + directory_cost(&relative);
                ensure!(
                    next_bytes <= MAX_DIRECTORY_BYTES,
                    LibraryError::budget("inotify directory metadata budget exceeded")
                );
                self.subscription_bytes = next_bytes;
                node.subscriptions.insert(index, relative);
                self.contexts[index].as_mut().unwrap().wds.insert(wd);
                return Ok(());
            }
            ensure!(
                self.subscriptions < MAX_SUBSCRIPTIONS
                    && self.subscription_bytes + directory_cost(&relative) <= MAX_DIRECTORY_BYTES,
                LibraryError::budget("inotify subscription budget exceeded")
            );
            self.subscription_bytes += directory_cost(&relative);
            node.subscriptions.insert(index, relative);
            self.subscriptions += 1;
            self.contexts[index].as_mut().unwrap().wds.insert(wd);
            return Ok(());
        }
        ensure!(
            self.nodes.len() < MAX_WDS
                && self.subscriptions < MAX_SUBSCRIPTIONS
                && self.subscription_bytes + directory_cost(&relative) <= MAX_DIRECTORY_BYTES,
            LibraryError::budget("inotify directory budget exceeded")
        );
        // Only an internally held, verified descriptor is followed through proc.
        let proc_name = CString::new(format!("/proc/self/fd/{}", file.as_raw_fd()))?;
        let retained = file.try_clone()?;
        let wd =
            unsafe { libc::inotify_add_watch(self.inotify.as_raw_fd(), proc_name.as_ptr(), MASK) };
        ensure!(
            wd >= 0,
            "Cannot subscribe verified inotify directory: {}",
            io::Error::last_os_error()
        );
        // Active physical duplicates are represented by a single node. A retired
        // wd is never accepted for a new subscription before IN_IGNORED drains.
        if self.nodes.contains_key(&wd) {
            self.rebuild_requested = true;
            return Err(LibraryError::new(
                LibraryErrorKind::Busy,
                "inotify descriptor reuse requires instance rebuild",
            )
            .into());
        }
        self.subscription_bytes += directory_cost(&relative);
        self.nodes.insert(
            wd,
            Node {
                _file: retained,
                dev,
                ino,
                subscriptions: BTreeMap::from([(index, relative)]),
                waiters: BTreeSet::new(),
                retiring: false,
            },
        );
        self.subscriptions += 1;
        self.contexts[index].as_mut().unwrap().wds.insert(wd);
        let after = file.metadata()?;
        ensure!(
            after.dev() == dev && after.ino() == ino && after.is_dir(),
            "inotify descriptor identity changed"
        );
        Ok(())
    }
    fn rebuild(&mut self, output: &mut Output) -> Result<()> {
        let raw = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
        ensure!(
            raw >= 0,
            "Cannot rebuild inotify owner: {}",
            io::Error::last_os_error()
        );
        let replacement = unsafe { File::from_raw_fd(raw) };
        drop(std::mem::replace(&mut self.inotify, replacement));
        self.nodes.clear();
        self.subscriptions = 0;
        self.subscription_bytes = 0;
        self.rebuild_requested = false;
        for context in self.contexts.iter_mut().flatten() {
            context.wds.clear();
            context.pending_ignored = 0;
            if !context.retiring {
                context.refresh = true;
                output.dirty(&context.dirty, Vec::new(), true, context.active);
            }
        }
        for index in 0..self.contexts.len() {
            if self.contexts[index].as_ref().is_some_and(|c| !c.retiring) {
                match self.discover(index) {
                    Ok(()) => self.contexts[index].as_mut().unwrap().refresh = false,
                    Err(error) => {
                        let context = self.contexts[index].as_ref().unwrap();
                        output.control(
                            NativeWatchEvent::Error {
                                token: context.token.clone(),
                                error: watch_error(&error),
                            },
                            None,
                        );
                    }
                }
            }
        }
        Ok(())
    }
    fn discover(&mut self, index: usize) -> Result<()> {
        let previous = std::mem::take(&mut self.contexts[index].as_mut().unwrap().wds);
        let result = self.discover_current(index);
        if result.is_err() {
            self.contexts[index].as_mut().unwrap().wds.extend(previous);
        } else {
            let current = self.contexts[index].as_ref().unwrap().wds.clone();
            for wd in previous.difference(&current) {
                self.detach(index, *wd);
            }
        }
        result
    }
    fn discover_current(&mut self, index: usize) -> Result<()> {
        let (authority, root_path, recursive) = {
            let context = self.contexts[index].as_ref().unwrap();
            (
                context.authority.clone(),
                context.last_path.clone(),
                context.root.recursive,
            )
        };
        let directory = authority.open_directory(&root_path)?;
        self.add(index, directory, PathBuf::new())?;
        if !recursive {
            return Ok(());
        }
        let mut pending = VecDeque::from([PathBuf::new()]);
        let mut pending_bytes = directory_cost(Path::new(""));
        let mut entries = 0usize;
        let mut directories = 1usize;
        let began = Instant::now();
        while let Some(relative) = pending.pop_front() {
            pending_bytes -= directory_cost(&relative);
            ensure!(
                !self.shared.closing.load(Ordering::Acquire),
                "Native watch is closing"
            );
            ensure!(
                began.elapsed() < Duration::from_secs(30),
                LibraryError::new(
                    LibraryErrorKind::Timeout,
                    "inotify discovery deadline exceeded"
                )
            );
            let absolute = root_path.join(&relative);
            let directory = authority.open_directory(&absolute)?;
            for entry in directory.read_dir()? {
                entries += 1;
                ensure!(
                    entries <= 250_000,
                    LibraryError::budget("inotify discovery entry budget exceeded")
                );
                let entry = entry?;
                // A user-controlled symlink is not followed or treated as a directory.
                if !entry.file_type()?.is_dir() {
                    continue;
                }
                let child = relative.join(entry.file_name());
                ensure!(
                    safe_relative(&child) && child.components().count() <= 64,
                    "Invalid inotify recursive directory"
                );
                directories += 1;
                ensure!(
                    directories <= MAX_WDS,
                    LibraryError::budget("inotify recursive directory budget exceeded")
                );
                let directory = authority.open_directory(root_path.join(&child))?;
                self.add(index, directory, child.clone())?;
                pending_bytes += directory_cost(&child);
                ensure!(
                    pending_bytes <= MAX_DIRECTORY_BYTES,
                    LibraryError::budget("inotify discovery metadata budget exceeded")
                );
                pending.push_back(child);
            }
        }
        Ok(())
    }
    fn detach(&mut self, index: usize, wd: i32) {
        let Some(node) = self.nodes.get_mut(&wd) else {
            return;
        };
        if let Some(relative) = node.subscriptions.remove(&index) {
            self.subscriptions -= 1;
            self.subscription_bytes -= directory_cost(&relative);
        }
        if node.subscriptions.is_empty() && !node.retiring {
            node.retiring = true;
            node.waiters.insert(index);
            self.contexts[index].as_mut().unwrap().pending_ignored += 1;
            // The owned instance remains live until IN_IGNORED is consumed.
            unsafe {
                libc::inotify_rm_watch(self.inotify.as_raw_fd(), wd);
            }
        }
    }
    fn cancel(&mut self, index: usize, output: &mut Output) {
        let Some(context) = self.contexts[index].as_mut() else {
            return;
        };
        if context.retiring {
            return;
        }
        context.retiring = true;
        context.active = false;
        let wds: Vec<i32> = context.wds.iter().copied().collect();
        for wd in wds {
            let Some(node) = self.nodes.get_mut(&wd) else {
                continue;
            };
            if let Some(relative) = node.subscriptions.remove(&index) {
                self.subscriptions -= 1;
                self.subscription_bytes -= directory_cost(&relative);
            }
            if node.subscriptions.is_empty() && !node.retiring {
                node.retiring = true;
                node.waiters.insert(index);
                context.pending_ignored += 1;
                let result = unsafe { libc::inotify_rm_watch(self.inotify.as_raw_fd(), wd) };
                if result != 0 {
                    let error = io::Error::last_os_error();
                    // EINVAL can mean that deletion already queued IN_IGNORED.
                    if error.raw_os_error() != Some(libc::EINVAL) {
                        output.control(
                            NativeWatchEvent::Error {
                                token: context.token.clone(),
                                error: watch_error(&anyhow::anyhow!(
                                    "Cannot remove inotify watch: {error}"
                                )),
                            },
                            None,
                        );
                    }
                }
            }
        }
    }
    fn retire(&mut self, output: &mut Output) {
        for slot in &mut self.contexts {
            if slot
                .as_ref()
                .is_some_and(|c| c.retiring && c.pending_ignored == 0)
            {
                let mut context = slot.take().unwrap();
                let root_id = context.token.root_id.clone();
                let generation = context.token.watch_generation;
                let stops = std::mem::take(&mut context.stops);
                drop(context);
                if !stops.is_empty() || output.shared.closing.load(Ordering::Acquire) {
                    output.forget(&root_id, generation);
                }
                for credit in stops {
                    output.control(
                        NativeWatchEvent::Stopped {
                            root_id: root_id.clone(),
                            watch_generation: generation,
                        },
                        Some(credit),
                    );
                }
            }
        }
    }
    fn consume(&mut self, output: &mut Output) -> Result<()> {
        let mut buffer = [0u32; 16 * 1024];
        // Bound native dequeue work so a sustained storm cannot starve Stop.
        for _ in 0..16 {
            let size = unsafe {
                libc::read(
                    self.inotify.as_raw_fd(),
                    buffer.as_mut_ptr().cast(),
                    std::mem::size_of_val(&buffer),
                )
            };
            if size < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::WouldBlock {
                    return Ok(());
                }
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error).context("Cannot consume inotify events");
            }
            if size == 0 {
                return Ok(());
            }
            let bytes =
                unsafe { std::slice::from_raw_parts(buffer.as_ptr().cast::<u8>(), size as usize) };
            let mut offset = 0usize;
            while offset < bytes.len() {
                ensure!(
                    offset + std::mem::size_of::<libc::inotify_event>() <= bytes.len(),
                    "Invalid inotify event header"
                );
                let event = unsafe { ptr_read_event(bytes.as_ptr().add(offset)) };
                let header = std::mem::size_of::<libc::inotify_event>();
                let length = event.len as usize;
                ensure!(
                    offset + header + length <= bytes.len(),
                    "Invalid inotify event name size"
                );
                let name = &bytes[offset + header..offset + header + length];
                offset += header + length;
                if event.mask & libc::IN_Q_OVERFLOW != 0 {
                    for context in self.contexts.iter_mut().flatten().filter(|c| !c.retiring) {
                        context.refresh = true;
                        output.dirty(&context.dirty, Vec::new(), true, context.active);
                        context.dirty.dropped.fetch_add(1, Ordering::AcqRel);
                    }
                    continue;
                }
                if event.mask & libc::IN_IGNORED != 0 {
                    #[cfg(test)]
                    self.shared
                        .cancelled_completions
                        .fetch_add(1, Ordering::AcqRel);
                    #[cfg(test)]
                    wait_ignored_gate(&self.shared);
                    if let Some(node) = self.nodes.remove(&event.wd) {
                        self.subscriptions =
                            self.subscriptions.saturating_sub(node.subscriptions.len());
                        self.subscription_bytes = self.subscription_bytes.saturating_sub(
                            node.subscriptions
                                .values()
                                .map(|relative| directory_cost(relative))
                                .sum(),
                        );
                        for index in node.waiters {
                            if let Some(context) = self.contexts[index].as_mut() {
                                context.pending_ignored = context.pending_ignored.saturating_sub(1);
                            }
                        }
                        for (index, relative) in node.subscriptions {
                            if let Some(context) = self.contexts[index].as_mut() {
                                context.wds.remove(&event.wd);
                                context.refresh = true;
                                output.dirty(&context.dirty, Vec::new(), true, context.active);
                                if relative.as_os_str().is_empty() {
                                    output.control(
                                        NativeWatchEvent::Error {
                                            token: context.token.clone(),
                                            error: LibraryError::new(
                                                LibraryErrorKind::Unavailable,
                                                "inotify root watch was removed",
                                            ),
                                        },
                                        None,
                                    );
                                }
                            }
                        }
                    }
                    continue;
                }
                let Some(node) = self.nodes.get(&event.wd) else {
                    continue;
                };
                if node.retiring {
                    continue;
                }
                let subscribers: Vec<_> = node
                    .subscriptions
                    .iter()
                    .map(|(index, relative)| (*index, relative.clone()))
                    .collect();
                let truncated = &name[..name.iter().position(|b| *b == 0).unwrap_or(name.len())];
                let kind = if event.mask & libc::IN_MOVED_FROM != 0 {
                    NativeWatchKind::RenamedOld
                } else if event.mask & libc::IN_MOVED_TO != 0 {
                    NativeWatchKind::RenamedNew
                } else if event.mask & libc::IN_CREATE != 0 {
                    NativeWatchKind::Created
                } else if event.mask & libc::IN_DELETE != 0 {
                    NativeWatchKind::Removed
                } else {
                    NativeWatchKind::Modified
                };
                let self_change = event.mask & (libc::IN_DELETE_SELF | libc::IN_MOVE_SELF) != 0;
                for (index, relative) in subscribers {
                    let Some(context) = self.contexts[index].as_mut() else {
                        continue;
                    };
                    let path = relative.join(OsStr::from_bytes(truncated));
                    let valid = !truncated.is_empty() && safe_relative(&path);
                    let directory_change = event.mask & libc::IN_ISDIR != 0;
                    if self_change || directory_change {
                        context.refresh = true;
                    }
                    let hints = if valid {
                        vec![NativeWatchHint {
                            path,
                            kind,
                            is_directory: Some(directory_change),
                            rename_cookie: (event.cookie != 0).then_some(event.cookie),
                        }]
                    } else {
                        Vec::new()
                    };
                    // Recursive directory rename/install creates a namespace gap.
                    let full = !valid
                        || self_change
                        || (directory_change && context.root.recursive)
                        || event.mask & (libc::IN_MOVED_FROM | libc::IN_MOVED_TO) != 0;
                    output.dirty(&context.dirty, hints, full, context.active);
                }
            }
        }
        Ok(())
    }
    fn probe(&mut self, output: &mut Output) {
        for index in 0..self.contexts.len() {
            let Some(context) = self.contexts[index].as_mut().filter(|c| !c.retiring) else {
                continue;
            };
            let check = context
                .authority
                .check()
                .and_then(|_| context.directory.native_file())
                .and_then(|file| {
                    let metadata = file.metadata()?;
                    ensure!(
                        format!("{:016x}", metadata.dev()) == context.root.anchor.volume
                            && format!("{:016x}", metadata.ino()) == context.root.anchor.file_id,
                        "NeedsReapproval: inotify anchor changed"
                    );
                    Ok(std::fs::read_link(format!(
                        "/proc/self/fd/{}",
                        file.as_raw_fd()
                    ))?)
                });
            match check {
                Ok(path) if path != context.last_path => {
                    context.last_path = path.clone();
                    if context.token.options.root_move_policy
                        == crate::domain::library_watch::RootMovePolicy::Pause
                    {
                        context.dirty.fail(&LibraryError::new(
                            LibraryErrorKind::Unavailable,
                            "Native watch root moved",
                        ));
                    }
                    output.dirty(&context.dirty, Vec::new(), true, context.active);
                    output.control(
                        NativeWatchEvent::RootMoved {
                            token: context.token.clone(),
                            current_path: path,
                            anchor: context.root.anchor.clone(),
                        },
                        None,
                    );
                    if context.token.options.root_move_policy
                        == crate::domain::library_watch::RootMovePolicy::Pause
                    {
                        self.cancel(index, output);
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
                    self.cancel(index, output);
                }
                _ => {
                    if context.refresh {
                        // Drain old wd removals before attempting any installation.
                        if self.nodes.values().any(|n| n.retiring) {
                            continue;
                        }
                        match self.discover(index) {
                            Ok(()) => {
                                let context = self.contexts[index].as_mut().unwrap();
                                context.refresh = false;
                                // Installation may have missed changes before a child wd existed.
                                output.dirty(&context.dirty, Vec::new(), true, context.active);
                            }
                            Err(error) => {
                                let context = self.contexts[index].as_ref().unwrap();
                                output.dirty(&context.dirty, Vec::new(), true, context.active);
                                output.control(
                                    NativeWatchEvent::Error {
                                        token: context.token.clone(),
                                        error: watch_error(&error),
                                    },
                                    None,
                                );
                            }
                        }
                    }
                }
            }
        }
    }
}
fn directory_cost(path: &Path) -> usize {
    path.as_os_str().len() * 4 + 256
}
impl Drop for Owner {
    fn drop(&mut self) {
        // The wake fd remains owned until all possible enqueue writers finish.
        let _guard = self
            .shared
            .wake_lock
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        self.shared.wake_fd.store(-1, Ordering::Release);
    }
}
unsafe fn ptr_read_event(pointer: *const u8) -> libc::inotify_event {
    unsafe { std::ptr::read_unaligned(pointer.cast::<libc::inotify_event>()) }
}
pub(super) fn run(
    authority: AuthorityRegistry,
    commands: Receiver<Command>,
    shared: Arc<Shared>,
    output: &mut Output,
) -> Result<()> {
    let mut owner = Owner::new(shared.clone())?;
    let mut probe_at = Instant::now();
    loop {
        output.flush();
        // Consume queued old-generation records before native watch admission.
        if let Err(error) = owner.consume(output) {
            // Closing the instance cancels every native subscription; it is not
            // equivalent to ignoring an rm_watch error while buffers remain live.
            shared.closing.store(true, Ordering::Release);
            output.control(
                NativeWatchEvent::OwnerError {
                    error: watch_error(&error),
                },
                None,
            );
            break;
        }
        owner.retire(output);
        for _ in 0..CONTROL_CAPACITY {
            let Ok(command) = commands.try_recv() else {
                break;
            };
            if shared.closing.load(Ordering::Acquire) {
                if let Command::Prepare { token, .. } = command {
                    output.forget(&token.root_id, token.watch_generation);
                }
                continue;
            }
            match command {
                Command::Prepare {
                    root,
                    token,
                    dirty,
                    slot,
                    credit,
                } => {
                    let index = owner
                        .contexts
                        .iter()
                        .position(Option::is_none)
                        .context("inotify root capacity exhausted")?;
                    let preparation = (|| -> Result<()> {
                        let verified = authority.watch_authority(&root)?;
                        let directory = verified.open_directory(&root.path)?;
                        owner.contexts[index] = Some(Context {
                            last_path: root.path.clone(),
                            root,
                            token: token.clone(),
                            authority: verified,
                            directory,
                            dirty,
                            _slot: slot,
                            active: false,
                            retiring: false,
                            pending_ignored: 0,
                            stops: Vec::new(),
                            wds: BTreeSet::new(),
                            refresh: false,
                        });
                        owner.discover(index)
                    })();
                    match preparation {
                        Ok(()) => {
                            output.control(
                                NativeWatchEvent::Prepared {
                                    token,
                                    result: Ok(()),
                                },
                                Some(credit),
                            );
                        }
                        Err(error) => {
                            if owner.contexts[index].is_some() {
                                owner.cancel(index, output);
                            } else {
                                output.forget(&token.root_id, token.watch_generation);
                            }
                            output.control(
                                NativeWatchEvent::Prepared {
                                    token,
                                    result: Err(error),
                                },
                                Some(credit),
                            );
                        }
                    }
                }
                Command::Activate { token, credit } => {
                    if let Some(context) = owner
                        .contexts
                        .iter_mut()
                        .flatten()
                        .find(|c| c.token == token && !c.retiring)
                    {
                        context.active = true;
                        output.control(NativeWatchEvent::Watching { token }, Some(credit));
                        output.baseline(&context.dirty);
                    } else {
                        output.control(
                            NativeWatchEvent::Error {
                                token,
                                error: LibraryError::conflict(
                                    "Prepared inotify generation is no longer owned",
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
                    if let Some(index) = owner.contexts.iter().position(|c| {
                        c.as_ref().is_some_and(|c| {
                            c.token.root_id == root_id && c.token.watch_generation == generation
                        })
                    }) {
                        owner.contexts[index].as_mut().unwrap().stops.push(credit);
                        owner.cancel(index, output);
                    } else {
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
        }
        if owner.rebuild_requested && !shared.closing.load(Ordering::Acquire) {
            owner.rebuild(output)?;
        }
        if shared.closing.load(Ordering::Acquire) {
            for index in 0..owner.contexts.len() {
                owner.cancel(index, output);
            }
        }
        owner.retire(output);
        if shared.closing.load(Ordering::Acquire) && owner.contexts.iter().all(Option::is_none) {
            break;
        }
        if probe_at.elapsed() >= Duration::from_secs(1) && !shared.closing.load(Ordering::Acquire) {
            probe_at = Instant::now();
            owner.probe(output);
        }
        let mut polls = [
            libc::pollfd {
                fd: owner.inotify.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: owner.wake.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        let result = unsafe { libc::poll(polls.as_mut_ptr(), 2, 10) };
        if result < 0 && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
            return Err(io::Error::last_os_error()).context("inotify owner poll failed");
        }
        if polls[1].revents & libc::POLLIN != 0 {
            let mut value = 0u64;
            unsafe {
                libc::read(owner.wake.as_raw_fd(), &mut value as *mut _ as *mut _, 8);
            }
        }
    }
    while let Ok(command) = commands.try_recv() {
        if let Command::Prepare { token, .. } = command {
            output.forget(&token.root_id, token.watch_generation);
        }
    }
    // Instance close and retained directory FH destruction occur on this owner.
    drop(owner);
    Ok(())
}
#[cfg(test)]
fn wait_ignored_gate(shared: &Shared) {
    let gate = {
        shared
            .gate
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    };
    if let Some(gate) = gate {
        gate.entered.store(true, Ordering::Release);
        while !gate.release.load(Ordering::Acquire) {
            std::thread::park_timeout(Duration::from_millis(1));
        }
    }
}
