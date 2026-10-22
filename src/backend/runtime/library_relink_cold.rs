//! Bounded namespace evidence for files replaced while a cold scan is running.
use crate::domain::{
    library::{
        FileIdentity, LibraryError, LibraryScanToken, MAX_LIBRARY_BATCH_ITEMS,
        validate_library_path,
    },
    library_watch::{LibraryAssetBinding, LibraryScanScope, LibraryWatchToken},
    music::normalize_local_path,
};
use anyhow::{Context, Result, ensure};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

const MAX_SCANS: usize = 10;
const MAX_BYTES: usize = 8 * 1024 * 1024;
const MAX_DESCRIPTORS: usize = 200;
const MAX_DESCRIPTOR_BYTES: usize = 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ColdReplacementMode {
    FreshIdentity,
    PreserveLogicalIdentity,
}
#[derive(Clone, Debug)]
pub(super) enum ColdAction {
    RelocateOriginal {
        original: LibraryAssetBinding,
        new_path: PathBuf,
    },
    ReopenReplacement {
        original: LibraryAssetBinding,
        path: PathBuf,
        observed_identity: FileIdentity,
        mode: ColdReplacementMode,
    },
    Unresolved {
        original: LibraryAssetBinding,
        reason: LibraryError,
    },
}
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct ColdObservation {
    pub suppress_replacement: bool,
}
#[derive(Debug)]
pub(super) struct ColdDecision {
    pub actions: Vec<ColdAction>,
    pub allow_missing_finalizer: bool,
}
#[derive(Default)]
pub(super) struct ColdScanFence {
    scans: BTreeMap<String, ColdScan>,
}
struct ColdScan {
    token: LibraryScanToken,
    watch: LibraryWatchToken,
    scope: LibraryScanScope,
    sequence: u64,
    dropped_events: u64,
    seen: BTreeSet<[u8; 32]>,
    originals: BTreeMap<String, FrozenOriginal>,
    identity_gap: bool,
    sealed: bool,
    seal_valid: bool,
}
struct FrozenOriginal {
    binding: LibraryAssetBinding,
    relocated_path: Option<PathBuf>,
    ambiguous: bool,
    original_locator_seen: bool,
    published: bool,
    replacement: Option<Replacement>,
}
struct Replacement {
    path: PathBuf,
    identity: Option<FileIdentity>,
    phase: ReplacementPhase,
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum ReplacementPhase {
    Discovering,
    AwaitPublication,
    AwaitReopen,
    Unresolved,
    Resolved,
}

impl ColdScanFence {
    pub(super) fn begin(
        &mut self,
        token: LibraryScanToken,
        watch: LibraryWatchToken,
        scope: LibraryScanScope,
        received_sequence: u64,
        dropped_events: u64,
    ) -> Result<()> {
        token.validate()?;
        watch.validate()?;
        scope.validate()?;
        ensure!(
            watch.enabled
                && watch.options.auto_relink
                && watch.root_id == token.root_id
                && watch.grant_epoch == token.grant_epoch,
            LibraryError::conflict("Cold scan watch grant does not match the scan")
        );
        ensure!(
            !self.scans.contains_key(&token.operation_id),
            LibraryError::conflict("Cold scan already owns its fence")
        );
        ensure!(
            self.scans.len() < MAX_SCANS,
            LibraryError::budget("Cold scan fence has ten active scans")
        );
        let scan = ColdScan {
            token,
            watch,
            scope,
            sequence: received_sequence,
            dropped_events,
            seen: BTreeSet::new(),
            originals: BTreeMap::new(),
            identity_gap: false,
            sealed: false,
            seal_valid: false,
        };
        let key = scan.token.operation_id.clone();
        self.scans.insert(key.clone(), scan);
        if let Err(error) = self.check_budget() {
            self.scans.remove(&key);
            return Err(error);
        }
        Ok(())
    }

    /// The caller supplies bindings read before any write of this batch, and
    /// filters identity matches to membership in the original root grant.
    pub(super) fn observe(
        &mut self,
        token: &LibraryScanToken,
        path: &Path,
        actual_identity: Option<&FileIdentity>,
        occupied_path_binding: Option<&LibraryAssetBinding>,
        original_identity_matches: &[LibraryAssetBinding],
    ) -> Result<ColdObservation> {
        validate_library_path(path)?;
        ensure!(
            original_identity_matches.len() <= MAX_LIBRARY_BATCH_ITEMS,
            LibraryError::budget("Cold scan identity match envelope is too large")
        );
        if let Some(identity) = actual_identity {
            identity.validate()?;
        }
        let path = normalize_local_path(path);
        if let Err(error) = self.admit_observation(
            token,
            &path,
            actual_identity,
            occupied_path_binding,
            original_identity_matches,
        ) {
            self.scans.remove(&token.operation_id);
            return Err(error);
        }
        let scan = self.scan_mut(token)?;
        ensure!(
            !scan.sealed,
            LibraryError::conflict("Cold scan namespace was already sealed")
        );
        if let Some(identity) = actual_identity {
            scan.seen.insert(identity_key(identity)?);
        } else {
            scan.identity_gap = true;
        }
        for binding in original_identity_matches {
            if binding.file_identity.as_ref() != actual_identity || actual_identity.is_none() {
                continue;
            }
            let old_path = normalize_local_path(&binding.path);
            if old_path == path {
                if let Some(original) = scan.originals.get_mut(&binding.asset_id) {
                    original.original_locator_seen = true;
                }
                continue;
            }
            let original = freeze(scan, binding)?;
            if let Some(previous) = &original.relocated_path {
                if previous != &path {
                    original.ambiguous = true;
                }
            } else {
                original.relocated_path = Some(path.clone());
            }
        }
        let mut suppress_replacement = false;
        if let Some(binding) = occupied_path_binding {
            ensure!(
                normalize_local_path(&binding.path) == path,
                LibraryError::conflict("Cold scan occupied binding has a different locator")
            );
            if binding.file_identity.as_ref() != actual_identity || actual_identity.is_none() {
                suppress_replacement = true;
                let original = freeze(scan, binding)?;
                if let Some(replacement) = &original.replacement {
                    if replacement.path != path || replacement.identity.as_ref() != actual_identity
                    {
                        scan.identity_gap = true;
                    }
                } else {
                    original.replacement = Some(Replacement {
                        path,
                        identity: actual_identity.cloned(),
                        phase: ReplacementPhase::Discovering,
                    });
                }
            }
        }
        if let Err(error) = self.check_budget() {
            // This namespace can no longer certify absence or missing files.
            // Removing it releases the metadata rather than retaining an
            // over-budget snapshot that cannot make progress.
            self.scans.remove(&token.operation_id);
            return Err(error);
        }
        Ok(ColdObservation {
            suppress_replacement,
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn seal(
        &mut self,
        token: &LibraryScanToken,
        current_watch: &LibraryWatchToken,
        discovery_complete: bool,
        skipped_files: usize,
        identity_errors: usize,
        current_sequence: u64,
        current_dropped_events: u64,
        native_healthy: bool,
    ) -> Result<ColdDecision> {
        let scan = self.scan_mut(token)?;
        ensure!(
            !scan.sealed,
            LibraryError::conflict("Cold scan namespace was already sealed")
        );
        scan.sealed = true;
        scan.seal_valid = discovery_complete
            && skipped_files == 0
            && identity_errors == 0
            && !scan.identity_gap
            && native_healthy
            && &scan.watch == current_watch
            && scan.sequence == current_sequence
            && scan.dropped_events == current_dropped_events;
        let root_complete = scan.seal_valid && scan.scope == LibraryScanScope::Root;
        let mut actions = Vec::new();
        for original in scan.originals.values_mut() {
            let Some(replacement) = original.replacement.as_mut() else {
                if original.published {
                    continue;
                }
                if scan.seal_valid
                    && !original.ambiguous
                    && !original.original_locator_seen
                    && let Some(path) = &original.relocated_path
                {
                    actions.push(ColdAction::RelocateOriginal {
                        original: original.binding.clone(),
                        new_path: path.clone(),
                    });
                } else {
                    actions.push(unresolved(
                        original,
                        "Original relocation has incomplete or ambiguous namespace evidence",
                    ));
                }
                continue;
            };
            if !scan.seal_valid
                || original.binding.file_identity.is_none()
                || replacement.identity.is_none()
            {
                replacement.phase = ReplacementPhase::Unresolved;
                actions.push(unresolved(
                    original,
                    "Cold scan has incomplete, stale or unknown object identity evidence",
                ));
                continue;
            }
            if let Some(path) = original.relocated_path.as_ref() {
                if original.ambiguous || original.original_locator_seen {
                    replacement.phase = ReplacementPhase::Unresolved;
                    actions.push(unresolved(
                        original,
                        "Original object has multiple observed locators",
                    ));
                } else if original.published {
                    replacement.phase = ReplacementPhase::AwaitReopen;
                    actions.push(reopen(original, ColdReplacementMode::FreshIdentity)?);
                } else {
                    replacement.phase = ReplacementPhase::AwaitPublication;
                    actions.push(ColdAction::RelocateOriginal {
                        original: original.binding.clone(),
                        new_path: path.clone(),
                    });
                }
            } else {
                let seen = scan.seen.contains(&identity_key(
                    original.binding.file_identity.as_ref().unwrap(),
                )?);
                if root_complete && !seen {
                    replacement.phase = ReplacementPhase::AwaitReopen;
                    actions.push(reopen(
                        original,
                        ColdReplacementMode::PreserveLogicalIdentity,
                    )?);
                } else {
                    replacement.phase = ReplacementPhase::Unresolved;
                    actions.push(unresolved(
                        original,
                        "Complete root discovery has not proved the original object absent",
                    ));
                }
            }
        }
        Ok(ColdDecision {
            actions,
            allow_missing_finalizer: scan.can_finalize(),
        })
    }

    /// Called only after Repository CAS and actual physical ticket publication.
    pub(super) fn published(
        &mut self,
        token: &LibraryScanToken,
        asset_id: &str,
        committed: &LibraryAssetBinding,
    ) -> Result<Option<ColdAction>> {
        let scan = self.scan_mut(token)?;
        let original = scan.originals.get_mut(asset_id).ok_or_else(|| {
            LibraryError::conflict("Published relocation has no frozen original binding")
        })?;
        ensure!(
            committed.asset_id == original.binding.asset_id
                && committed.binding_revision
                    == original
                        .binding
                        .binding_revision
                        .checked_add(1)
                        .ok_or_else(|| LibraryError::conflict(
                            "Cold scan binding revision overflow"
                        ))?
                && committed.file_identity == original.binding.file_identity
                && original
                    .relocated_path
                    .as_ref()
                    .is_some_and(|path| normalize_local_path(&committed.path) == *path),
            LibraryError::conflict(
                "Published relocation differs from its frozen object, revision or locator"
            )
        );
        if let Some(old_hash) = &original.binding.content_sha256 {
            ensure!(
                committed
                    .content_sha256
                    .as_ref()
                    .is_some_and(|hash| hash.eq_ignore_ascii_case(old_hash)),
                LibraryError::conflict(
                    "Published relocation changed the frozen original content evidence"
                )
            );
        }
        original.published = true;
        if let Some(replacement) = original.replacement.as_mut()
            && replacement.phase == ReplacementPhase::AwaitPublication
        {
            replacement.phase = ReplacementPhase::AwaitReopen;
            return Ok(Some(reopen(original, ColdReplacementMode::FreshIdentity)?));
        }
        Ok(None)
    }

    /// Called after the fresh root-opened preparation is actually committed.
    pub(super) fn resolve_replacement(
        &mut self,
        token: &LibraryScanToken,
        asset_id: &str,
    ) -> Result<bool> {
        let scan = self.scan_mut(token)?;
        let replacement = scan
            .originals
            .get_mut(asset_id)
            .and_then(|original| original.replacement.as_mut())
            .ok_or_else(|| LibraryError::conflict("Cold scan replacement is unavailable"))?;
        ensure!(
            replacement.phase == ReplacementPhase::AwaitReopen,
            LibraryError::conflict("Cold scan replacement has no admitted fresh reopen")
        );
        replacement.phase = ReplacementPhase::Resolved;
        Ok(scan.can_finalize())
    }
    pub(super) fn can_finalize(&self, token: &LibraryScanToken) -> bool {
        self.scans
            .get(&token.operation_id)
            .is_some_and(|scan| scan.token == *token && scan.can_finalize())
    }
    pub(super) fn can_cover_watch(&self, token: &LibraryScanToken) -> bool {
        self.scans.get(&token.operation_id).is_some_and(|scan| {
            scan.token == *token
                && scan.sealed
                && scan.seal_valid
                && scan.scope == LibraryScanScope::Root
        })
    }
    pub(super) fn contains_original(&self, token: &LibraryScanToken, asset_id: &str) -> bool {
        self.scans
            .get(&token.operation_id)
            .is_some_and(|scan| scan.token == *token && scan.originals.contains_key(asset_id))
    }
    pub(super) fn owns(&self, token: &LibraryScanToken) -> bool {
        self.scans
            .get(&token.operation_id)
            .is_some_and(|scan| scan.token == *token)
    }
    pub(super) fn remove(&mut self, token: &LibraryScanToken) {
        if self
            .scans
            .get(&token.operation_id)
            .is_some_and(|scan| scan.token == *token)
        {
            self.scans.remove(&token.operation_id);
        }
    }
    pub(super) fn abort(&mut self, token: &LibraryScanToken) {
        self.remove(token);
    }
    pub(super) fn retire_root(&mut self, root_id: &str) {
        self.scans.retain(|_, scan| scan.token.root_id != root_id);
    }
    fn scan_mut(&mut self, token: &LibraryScanToken) -> Result<&mut ColdScan> {
        let scan = self
            .scans
            .get_mut(&token.operation_id)
            .ok_or_else(|| LibraryError::conflict("Cold scan fence is unavailable or exhausted"))?;
        ensure!(
            scan.token == *token,
            LibraryError::conflict("Cold scan token changed")
        );
        Ok(scan)
    }
    fn usage(&self) -> Result<(usize, usize, usize)> {
        let mut bytes = std::mem::size_of::<Self>();
        let mut descriptor_bytes = 0usize;
        let mut descriptors = 0usize;
        for scan in self.scans.values() {
            bytes = bytes.saturating_add(
                1024 + scan.token.root_id.capacity()
                    + scan.token.operation_id.capacity()
                    + scan.watch.root_id.capacity()
                    + serde_json::to_vec(&scan.scope)?.len() * 2
                    + scan.seen.len().saturating_mul(64),
            );
            for (key, original) in &scan.originals {
                descriptors += 1;
                let mut owned = 512
                    + key.capacity()
                    + binding_bytes(&original.binding)
                    + original
                        .relocated_path
                        .as_ref()
                        .map_or(0, |path| path.capacity() * 2);
                if let Some(replacement) = &original.replacement {
                    owned += 256
                        + replacement.path.capacity() * 2
                        + replacement.identity.as_ref().map_or(0, identity_bytes);
                }
                descriptor_bytes = descriptor_bytes.saturating_add(owned);
                bytes = bytes.saturating_add(owned);
            }
        }
        Ok((bytes, descriptors, descriptor_bytes))
    }
    fn check_budget(&self) -> Result<()> {
        let (bytes, descriptors, descriptor_bytes) = self.usage()?;
        ensure!(
            descriptors <= MAX_DESCRIPTORS && descriptor_bytes <= MAX_DESCRIPTOR_BYTES,
            LibraryError::budget("Cold scan replacement descriptors exceed 200 or 1 MiB")
        );
        ensure!(
            bytes <= MAX_BYTES,
            LibraryError::budget("Cold scan identity evidence exceeds 8 MiB")
        );
        Ok(())
    }
    fn admit_observation(
        &self,
        token: &LibraryScanToken,
        path: &PathBuf,
        identity: Option<&FileIdentity>,
        occupied: Option<&LibraryAssetBinding>,
        matches: &[LibraryAssetBinding],
    ) -> Result<()> {
        let scan = self
            .scans
            .get(&token.operation_id)
            .ok_or_else(|| LibraryError::conflict("Cold scan fence is unavailable"))?;
        ensure!(
            scan.token == *token,
            LibraryError::conflict("Cold scan token changed")
        );
        let (mut bytes, mut count, mut descriptor_bytes) = self.usage()?;
        if identity
            .map(identity_key)
            .transpose()?
            .is_some_and(|key| !scan.seen.contains(&key))
        {
            // A bounded tree node can be added without reallocating a complete
            // hash table while the old allocation remains live.
            bytes = bytes.saturating_add(1024);
        }
        for binding in matches {
            if identity.is_none()
                || binding.file_identity.as_ref() != identity
                || normalize_local_path(&binding.path) == *path
            {
                continue;
            }
            let cost = if let Some(original) = scan.originals.get(&binding.asset_id) {
                if original.relocated_path.is_none() {
                    path.capacity() * 2 + 256
                } else {
                    0
                }
            } else {
                count += 1;
                1024 + binding_bytes(binding) + binding.asset_id.capacity() + path.capacity() * 2
            };
            bytes = bytes.saturating_add(cost);
            descriptor_bytes = descriptor_bytes.saturating_add(cost);
        }
        if let Some(binding) = occupied
            && (identity.is_none() || binding.file_identity.as_ref() != identity)
        {
            let snapshot = if scan.originals.contains_key(&binding.asset_id) {
                0
            } else {
                count += 1;
                1024 + binding_bytes(binding) + binding.asset_id.capacity()
            };
            let replacement = if scan
                .originals
                .get(&binding.asset_id)
                .is_some_and(|original| original.replacement.is_some())
            {
                0
            } else {
                512 + path.capacity() * 2 + identity.map_or(0, identity_bytes)
            };
            bytes = bytes.saturating_add(snapshot + replacement);
            descriptor_bytes = descriptor_bytes.saturating_add(snapshot + replacement);
        }
        ensure!(
            count <= MAX_DESCRIPTORS
                && descriptor_bytes <= MAX_DESCRIPTOR_BYTES
                && bytes <= MAX_BYTES,
            LibraryError::budget("Cold scan observation exceeds the admitted metadata budget")
        );
        Ok(())
    }
}
impl ColdScan {
    fn can_finalize(&self) -> bool {
        self.sealed
            && self.seal_valid
            && self.originals.values().all(|original| {
                original
                    .replacement
                    .as_ref()
                    .map_or(original.published, |replacement| {
                        replacement.phase == ReplacementPhase::Resolved
                    })
            })
    }
}
fn freeze<'a>(
    scan: &'a mut ColdScan,
    binding: &LibraryAssetBinding,
) -> Result<&'a mut FrozenOriginal> {
    validate_library_path(&binding.path)?;
    if let Some(identity) = &binding.file_identity {
        identity.validate()?;
    }
    if let Some(original) = scan.originals.get(&binding.asset_id) {
        ensure!(
            &original.binding == binding,
            LibraryError::conflict(
                "Original binding changed while cold namespace discovery was pending"
            )
        );
    } else {
        scan.originals.insert(
            binding.asset_id.clone(),
            FrozenOriginal {
                binding: binding.clone(),
                relocated_path: None,
                ambiguous: false,
                original_locator_seen: false,
                published: false,
                replacement: None,
            },
        );
    }
    Ok(scan.originals.get_mut(&binding.asset_id).unwrap())
}
fn identity_key(identity: &FileIdentity) -> Result<[u8; 32]> {
    identity.validate()?;
    Ok(Sha256::digest(serde_json::to_vec(identity)?).into())
}
fn binding_bytes(binding: &LibraryAssetBinding) -> usize {
    std::mem::size_of::<LibraryAssetBinding>()
        + binding.asset_id.capacity()
        + binding.path.capacity() * 2
        + binding.file_identity.as_ref().map_or(0, identity_bytes)
        + binding.content_sha256.as_ref().map_or(0, String::capacity)
}
fn identity_bytes(identity: &FileIdentity) -> usize {
    std::mem::size_of::<FileIdentity>()
        + identity.kind.capacity()
        + identity.volume.capacity()
        + identity.file_id.capacity()
}
fn unresolved(original: &FrozenOriginal, reason: &str) -> ColdAction {
    ColdAction::Unresolved {
        original: original.binding.clone(),
        reason: LibraryError::conflict(reason),
    }
}
fn reopen(original: &FrozenOriginal, mode: ColdReplacementMode) -> Result<ColdAction> {
    let replacement = original
        .replacement
        .as_ref()
        .context("Cold scan replacement is unavailable")?;
    Ok(ColdAction::ReopenReplacement {
        original: original.binding.clone(),
        path: replacement.path.clone(),
        observed_identity: replacement
            .identity
            .clone()
            .context("Cold scan replacement identity is unavailable")?,
        mode,
    })
}

#[cfg(all(test, any(windows, target_os = "linux")))]
mod tests {
    use super::*;
    use crate::{
        domain::library_watch::LibraryWatchOptions,
        library::{authority::RootAuthority, scan::ScanContext},
    };
    use std::{fs, time::Duration};

    fn tokens(index: usize) -> (LibraryScanToken, LibraryWatchToken) {
        let root_id = format!("root:cold:{index}");
        (
            LibraryScanToken {
                root_id: root_id.clone(),
                grant_epoch: 1,
                generation: 1,
                operation_id: format!("scan:cold:{index}"),
            },
            LibraryWatchToken {
                root_id,
                grant_epoch: 1,
                watch_generation: 1,
                enabled: true,
                options: LibraryWatchOptions {
                    auto_relink: true,
                    ..Default::default()
                },
            },
        )
    }
    fn binding(root: &RootAuthority, path: &Path, id: &str) -> LibraryAssetBinding {
        let opened = root.open_file(path).unwrap();
        LibraryAssetBinding {
            asset_id: id.into(),
            path: normalize_local_path(path),
            binding_revision: 3,
            file_identity: Some(opened.identity().clone()),
            content_sha256: Some(format!("{:x}", Sha256::digest(fs::read(path).unwrap()))),
            hash_size_bytes: Some(opened.metadata().unwrap().len()),
            hash_modified_ms: None,
        }
    }
    #[test]
    fn complete_hardlink_namespace_covers_watch_sequence_without_finalizing_original_identity() {
        let temp = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
        let old_path = temp.path().join("original.wav");
        let first = temp.path().join("one.wav");
        let second = temp.path().join("two.wav");
        fs::write(&old_path, b"original body").unwrap();
        let root = RootAuthority::open(
            temp.path(),
            true,
            &ScanContext::new(Duration::from_secs(30)),
        )
        .unwrap();
        let original = binding(&root, &old_path, "asset:ambiguous");
        fs::rename(&old_path, &first).unwrap();
        fs::hard_link(&first, &second).unwrap();
        let (token, watch) = tokens(0);
        let mut fence = ColdScanFence::default();
        fence
            .begin(token.clone(), watch.clone(), LibraryScanScope::Root, 7, 0)
            .unwrap();
        for path in [&first, &second] {
            let identity = root.open_file(path).unwrap().identity().clone();
            fence
                .observe(
                    &token,
                    path,
                    Some(&identity),
                    None,
                    std::slice::from_ref(&original),
                )
                .unwrap();
        }
        let sealed = fence.seal(&token, &watch, true, 0, 0, 7, 0, true).unwrap();
        assert!(!sealed.allow_missing_finalizer);
        assert!(matches!(
            sealed.actions.as_slice(),
            [ColdAction::Unresolved { .. }]
        ));
        assert!(fence.can_cover_watch(&token));
        assert!(!fence.can_finalize(&token));
        let barrier = root.retirement_barrier();
        drop(root);
        assert!(barrier.wait(Duration::from_secs(3)));
    }
    #[test]
    fn frozen_original_waits_for_verified_publication_and_a_fresh_replacement_commit() {
        let temp = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
        let old_path = temp.path().join("owned.wav");
        let moved = temp.path().join("late.wav");
        fs::write(&old_path, b"original body").unwrap();
        let root = RootAuthority::open(
            temp.path(),
            true,
            &ScanContext::new(Duration::from_secs(30)),
        )
        .unwrap();
        let old = binding(&root, &old_path, "asset:original");
        fs::rename(&old_path, &moved).unwrap();
        fs::write(&old_path, b"replacement body").unwrap();
        let new_identity = root.open_file(&old_path).unwrap().identity().clone();
        let moved_identity = root.open_file(&moved).unwrap().identity().clone();
        let (token, watch) = tokens(0);
        let mut fence = ColdScanFence::default();
        fence
            .begin(token.clone(), watch.clone(), LibraryScanScope::Root, 7, 0)
            .unwrap();
        assert!(
            fence
                .observe(&token, &old_path, Some(&new_identity), Some(&old), &[])
                .unwrap()
                .suppress_replacement
        );
        fence
            .observe(
                &token,
                &moved,
                Some(&moved_identity),
                None,
                std::slice::from_ref(&old),
            )
            .unwrap();
        let sealed = fence.seal(&token, &watch, true, 0, 0, 7, 0, true).unwrap();
        assert!(!sealed.allow_missing_finalizer);
        let ColdAction::RelocateOriginal { original, new_path } = &sealed.actions[0] else {
            panic!("Expected strong original relocation");
        };
        assert_eq!(original, &old);
        assert_eq!(new_path, &normalize_local_path(&moved));
        let mut committed = old.clone();
        committed.path = normalize_local_path(&moved);
        committed.binding_revision += 1;
        let mut wrong = committed.clone();
        wrong.file_identity = Some(new_identity.clone());
        assert!(fence.published(&token, &old.asset_id, &wrong).is_err());
        let ColdAction::ReopenReplacement {
            original,
            observed_identity,
            mode,
            ..
        } = fence
            .published(&token, &old.asset_id, &committed)
            .unwrap()
            .unwrap()
        else {
            panic!("Expected fresh identity allocation after actual publication");
        };
        assert_eq!(original, old);
        assert_eq!(observed_identity, new_identity);
        assert_eq!(mode, ColdReplacementMode::FreshIdentity);
        assert!(!fence.can_finalize(&token));
        assert!(fence.resolve_replacement(&token, &old.asset_id).unwrap());
        fence.remove(&token);
        assert!(!fence.can_finalize(&token));
        let barrier = root.retirement_barrier();
        drop(root);
        assert!(barrier.wait(Duration::from_secs(3)));
    }
    #[test]
    fn absence_requires_complete_root_identity_coverage_and_the_exact_native_watch_fence() {
        let temp = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
        let old_path = temp.path().join("owned.wav");
        let staged = temp.path().join("staged.wav");
        fs::write(&old_path, b"original body").unwrap();
        fs::write(&staged, b"new body").unwrap();
        let root = RootAuthority::open(
            temp.path(),
            true,
            &ScanContext::new(Duration::from_secs(30)),
        )
        .unwrap();
        let old = binding(&root, &old_path, "asset:slot");
        fs::remove_file(&old_path).unwrap();
        fs::rename(&staged, &old_path).unwrap();
        let actual = root.open_file(&old_path).unwrap().identity().clone();
        for case in 0..7 {
            let (token, watch) = tokens(case);
            let mut fence = ColdScanFence::default();
            let scope = if case == 4 {
                LibraryScanScope::Paths {
                    paths: vec![PathBuf::from("owned.wav")],
                }
            } else {
                LibraryScanScope::Root
            };
            fence
                .begin(token.clone(), watch.clone(), scope, 9, 1)
                .unwrap();
            let identity = if case == 5 { None } else { Some(&actual) };
            assert!(
                fence
                    .observe(&token, &old_path, identity, Some(&old), &[])
                    .unwrap()
                    .suppress_replacement
            );
            let mut current = watch.clone();
            if case == 6 {
                current.watch_generation += 1;
            }
            let decision = fence
                .seal(
                    &token,
                    &current,
                    case != 1,
                    usize::from(case == 2),
                    0,
                    if case == 3 { 10 } else { 9 },
                    1,
                    true,
                )
                .unwrap();
            assert!(!decision.allow_missing_finalizer);
            if case == 0 {
                assert!(matches!(
                    &decision.actions[0],
                    ColdAction::ReopenReplacement {
                        mode: ColdReplacementMode::PreserveLogicalIdentity,
                        ..
                    }
                ));
                assert!(fence.resolve_replacement(&token, &old.asset_id).unwrap());
            } else {
                assert!(matches!(
                    &decision.actions[0],
                    ColdAction::Unresolved { .. }
                ));
                assert!(!fence.can_finalize(&token));
            }
        }
        let barrier = root.retirement_barrier();
        drop(root);
        assert!(barrier.wait(Duration::from_secs(3)));
    }
    #[test]
    fn ten_scans_and_two_hundred_real_replacement_descriptors_have_exact_admission() {
        let mut fence = ColdScanFence::default();
        for index in 0..10 {
            let (token, watch) = tokens(index);
            fence
                .begin(token, watch, LibraryScanScope::Root, 0, 0)
                .unwrap();
        }
        let (token, watch) = tokens(10);
        assert!(
            fence
                .begin(token, watch, LibraryScanScope::Root, 0, 0)
                .is_err()
        );
        for index in 1..10 {
            fence.remove(&tokens(index).0);
        }
        let token = tokens(0).0;
        let temp = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
        let root = RootAuthority::open(
            temp.path(),
            true,
            &ScanContext::new(Duration::from_secs(30)),
        )
        .unwrap();
        for index in 0..201 {
            let path = temp.path().join(format!("slot-{index}.wav"));
            let archive = temp.path().join(format!("archive-{index}.wav"));
            fs::write(&path, b"original body").unwrap();
            let old = binding(&root, &path, &format!("asset:{index}"));
            fs::rename(&path, &archive).unwrap();
            fs::write(&path, b"replacement body").unwrap();
            let actual = root.open_file(&path).unwrap().identity().clone();
            let result = fence.observe(&token, &path, Some(&actual), Some(&old), &[]);
            if index < 200 {
                assert!(result.unwrap().suppress_replacement);
            } else {
                assert!(result.is_err());
            }
        }
        assert!(!fence.can_finalize(&token));
        assert!(fence.scans.is_empty());
        let barrier = root.retirement_barrier();
        drop(root);
        assert!(barrier.wait(Duration::from_secs(3)));
    }
}
