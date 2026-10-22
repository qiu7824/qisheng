//! Journaled allocation of the fixed, profile-owned impulse namespace.
//! The coordinator retains this creator across every failed journal or syscall.
use super::*;
use crate::library::authority::AuthorizedFile;
use std::io;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImpulseVaultCreationPhase {
    Intended,
    DirectoryPinned,
    OwnerAllocated,
    Ready,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImpulseVaultCreationFact {
    pub namespace: String,
    pub profile_identity: ImpulseFileIdentity,
    pub directory_identity: Option<ImpulseFileIdentity>,
    pub owner_identity: Option<ImpulseFileIdentity>,
    pub reserved_bytes: u64,
    pub phase: ImpulseVaultCreationPhase,
}
impl ImpulseVaultCreationFact {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            hex(&self.namespace, 32) && self.reserved_bytes == OWNER_BYTES as u64,
            "Invalid impulse namespace creation inventory"
        );
        self.profile_identity.validate()?;
        if let Some(identity) = &self.directory_identity {
            identity.validate()?;
        }
        if let Some(identity) = &self.owner_identity {
            identity.validate()?;
        }
        ensure!(
            self.phase == ImpulseVaultCreationPhase::Intended || self.directory_identity.is_some(),
            "Impulse namespace phase lacks its actual directory identity"
        );
        ensure!(
            matches!(
                self.phase,
                ImpulseVaultCreationPhase::Intended | ImpulseVaultCreationPhase::DirectoryPinned
            ) || self.owner_identity.is_some(),
            "Impulse namespace phase lacks its original owner identity"
        );
        Ok(())
    }
}

pub(crate) struct ImpulseVaultCreator {
    profile: SnapshotDirectory,
    directory: Option<SnapshotDirectory>,
    owner: Option<File>,
    owner_locked: bool,
    cold_unattested_directory: bool,
    allocated_directory: Option<File>,
    directory_selection: Option<SnapshotDirectory>,
    owner_grant: Option<crate::library::authority::FileAuthorization>,
    #[cfg(test)]
    obstruction_after_allocate: Option<Vec<u8>>,
    fact: ImpulseVaultCreationFact,
    limiter: SnapshotLimiter,
}
impl ImpulseVaultCreator {
    pub(crate) fn prepare(profile: &SnapshotDirectory) -> Result<Self> {
        profile.verify()?;
        Ok(Self {
            profile: profile.clone(),
            directory: None,
            owner: None,
            owner_locked: false,
            cold_unattested_directory: false,
            allocated_directory: None,
            directory_selection: None,
            owner_grant: None,
            #[cfg(test)]
            obstruction_after_allocate: None,
            fact: ImpulseVaultCreationFact {
                namespace: random_token()?,
                profile_identity: ImpulseFileIdentity::from_native(profile.identity()),
                directory_identity: None,
                owner_identity: None,
                reserved_bytes: OWNER_BYTES as u64,
                phase: ImpulseVaultCreationPhase::Intended,
            },
            limiter: retirement_limiter()?,
        })
    }
    pub(crate) fn resume(
        profile: &SnapshotDirectory,
        fact: &ImpulseVaultCreationFact,
    ) -> Result<Self> {
        fact.validate()?;
        profile.verify()?;
        ensure!(
            fact.profile_identity.matches(profile.identity()),
            "Creation inventory identifies another actual profile"
        );
        Ok(Self {
            profile: profile.clone(),
            directory: None,
            owner: None,
            owner_locked: false,
            cold_unattested_directory: fact.directory_identity.is_none(),
            allocated_directory: None,
            directory_selection: None,
            owner_grant: None,
            #[cfg(test)]
            obstruction_after_allocate: None,
            fact: fact.clone(),
            limiter: retirement_limiter()?,
        })
    }
    #[cfg(test)]
    pub(crate) fn fact(&self) -> &ImpulseVaultCreationFact {
        &self.fact
    }
    #[cfg(windows)]
    fn check_owner_grant(&self) -> Result<()> {
        match (&self.owner_grant, &self.owner) {
            (Some(grant), Some(owner)) => grant.check_retained_file(owner),
            (Some(grant), None) => grant.check(),
            (None, _) => Ok(()),
        }
    }
    fn pin_existing_directory(&mut self) -> Result<()> {
        if let Some(directory) = &self.directory {
            directory.verify()?;
            return Ok(());
        }
        let directory = SnapshotDirectory::open(
            &self.profile.path().join(DIRECTORY),
            self.profile.epoch(),
            super::super::DirectoryPurpose::Private,
        )?;
        platform::validate_private_directory(directory.parent())?;
        if let Some(expected) = &self.fact.directory_identity {
            ensure!(
                expected.matches(directory.identity()),
                "Creation inventory namespace was replaced"
            );
        }
        self.directory = Some(directory);
        Ok(())
    }
    #[cfg(windows)]
    pub(crate) fn advance(
        &mut self,
        context: &ImpulseDecodeContext,
        journal: &mut dyn FnMut(&ImpulseVaultCreationFact) -> Result<()>,
    ) -> Result<()> {
        context.check()?;
        self.profile.verify()?;
        self.fact.validate()?;
        if let Some(selection) = &self.directory_selection {
            selection.require_write(super::super::DirectoryPurpose::Selected)?;
        }
        self.check_owner_grant()?;
        // The actor durably records the allocation intent before any namespace
        // or leaf creation. Failing this acknowledgement creates no object.
        journal(&self.fact)?;
        if self.cold_unattested_directory {
            if self.directory.is_none() {
                match self.pin_existing_directory() {
                    Ok(()) => {}
                    Err(error)
                        if error.chain().any(|error| {
                            error
                                .downcast_ref::<io::Error>()
                                .is_some_and(|error| error.kind() == io::ErrorKind::NotFound)
                        }) =>
                    {
                        // Retain the actual exclusive allocation before identity
                        // capture, pinning or any later fallible acknowledgement.
                        self.allocated_directory = Some(platform::create_private_directory(
                            self.profile.parent(),
                            DIRECTORY,
                        )?);
                        self.pin_existing_directory()?;
                    }
                    Err(error) => return Err(error),
                }
            }
            let allocated = self.allocated_directory.as_ref().context(
                "Unknown impulse directory identity requires a fresh actual directory selection",
            )?;
            ensure!(
                platform::identity(allocated)? == *self.directory.as_ref().unwrap().identity(),
                "Pinned impulse directory differs from its actual exclusive allocation"
            );
            self.cold_unattested_directory = false;
        }
        if self.directory.is_none() {
            if self.fact.directory_identity.is_none() && self.allocated_directory.is_none() {
                self.allocated_directory = Some(platform::create_private_directory(
                    self.profile.parent(),
                    DIRECTORY,
                )?);
            }
            self.pin_existing_directory()?;
        }
        let directory = self.directory.as_ref().unwrap();
        if let Some(allocated) = &self.allocated_directory {
            ensure!(
                platform::identity(allocated)? == *directory.identity(),
                "Impulse namespace pin differs from its retained exclusive allocation"
            );
        }
        #[cfg(test)]
        if let Some(body) = self.obstruction_after_allocate.take() {
            fs::write(directory.path().join("foreign.txt"), body)?;
        }
        if self.fact.directory_identity.is_none() {
            ensure!(
                fs::read_dir(directory.path())?.next().is_none(),
                "Unattested impulse namespace is nonempty and needs an explicit current selection"
            );
            self.fact.directory_identity =
                Some(ImpulseFileIdentity::from_native(directory.identity()));
            self.fact.phase = ImpulseVaultCreationPhase::DirectoryPinned;
            journal(&self.fact)?;
        }
        context.check()?;
        if let Some(selection) = &self.directory_selection {
            selection.require_write(super::super::DirectoryPurpose::Selected)?;
        }
        self.check_owner_grant()?;
        let directory = self.directory.as_ref().unwrap();
        directory.verify()?;
        if self.owner.is_none() {
            // Retain the actual allocation immediately; every later fallible
            // identity, lock, journal, write or sync step leaves it in self.
            self.owner =
                Some(directory.open_group_file(OWNER, self.fact.owner_identity.is_none())?);
        }
        let owner = self.owner.as_mut().unwrap();
        let identity = directory.group_file_identity(owner)?;
        if let Some(expected) = &self.fact.owner_identity {
            ensure!(
                expected.matches(&identity),
                "Creation inventory owner was replaced"
            );
        } else {
            self.fact.owner_identity = Some(ImpulseFileIdentity::from_native(&identity));
            self.fact.phase = ImpulseVaultCreationPhase::OwnerAllocated;
        }
        if !self.owner_locked {
            owner
                .try_lock()
                .context("Impulse namespace has another producer")?;
            self.owner_locked = true;
        }
        // A crash after this acknowledgement has exact native owner FID proof.
        journal(&self.fact)?;
        context.check()?;
        let expected = owner_header(&self.fact.namespace);
        let length = owner.metadata()?.len();
        ensure!(
            length <= OWNER_BYTES as u64,
            "Creation owner exceeds its admitted body length"
        );
        let mut previous = [0; OWNER_BYTES];
        read_exact_at(owner, 0, &mut previous[..length as usize])?;
        ensure!(
            previous[..length as usize] == expected[..length as usize],
            "Creation owner body is foreign to the original namespace header"
        );
        owner.seek(SeekFrom::Start(0))?;
        owner.write_all(&expected)?;
        owner.sync_all()?;
        platform::sync_directory(directory.parent())?;
        context.check()?;
        ensure!(
            directory.group_file_identity(owner)? == identity
                && owner.metadata()?.len() == OWNER_BYTES as u64,
            "Creation owner changed before its durable ready acknowledgement"
        );
        let mut confirmation = [0; OWNER_BYTES];
        read_exact_at(owner, 0, &mut confirmation)?;
        ensure!(
            confirmation == expected,
            "Creation owner header differs from its original allocation"
        );
        directory.verify_group_name(OWNER, &identity)?;
        directory.verify()?;
        self.profile.verify()?;
        self.fact.phase = ImpulseVaultCreationPhase::Ready;
        journal(&self.fact)
    }
    #[cfg(not(windows))]
    pub(crate) fn advance(
        &mut self,
        _: &ImpulseDecodeContext,
        _: &mut dyn FnMut(&ImpulseVaultCreationFact) -> Result<()>,
    ) -> Result<()> {
        anyhow::bail!("Impulse namespace creation requires native Windows ownership")
    }

    /// Unknown-FID crash allocations require a fresh explicit file selection
    /// and write approval. The actual authorized read handle must identify the
    /// exact writable owner inside the continuously pinned profile namespace.
    #[cfg(all(test, windows))]
    pub(crate) fn reapprove_owner(
        &mut self,
        selected: AuthorizedFile,
        write_approved: bool,
        context: &ImpulseDecodeContext,
    ) -> Result<()> {
        ensure!(
            write_approved,
            "Impulse namespace recovery requires explicit owner-write approval"
        );
        context.check()?;
        self.profile.verify()?;
        self.pin_existing_directory()?;
        self.adopt_selected_owner(selected, context)
    }

    pub(crate) fn has_retained_owner(&self) -> bool {
        self.owner.is_some()
    }

    /// The fresh selection is an actual write-scoped native descriptor. JSON
    /// paths and creation facts never construct this authority.
    #[cfg(windows)]
    pub(crate) fn reapprove_namespace(
        &mut self,
        selected_directory: SnapshotDirectory,
        selected_owner: Option<AuthorizedFile>,
        context: &ImpulseDecodeContext,
    ) -> Result<()> {
        context.check()?;
        self.profile.verify()?;
        selected_directory.require_write(super::super::DirectoryPurpose::Selected)?;
        self.pin_existing_directory()?;
        let directory = self.directory.as_ref().unwrap();
        ensure!(
            selected_directory.identity() == directory.identity(),
            "Selected directory is not the current profile impulse namespace"
        );
        let mut entries = 0;
        for entry in fs::read_dir(directory.path())? {
            ensure!(
                entry?.file_name() == OWNER && entries == 0,
                "Unattested impulse namespace contains a foreign object"
            );
            entries += 1;
        }
        if entries == 0 {
            ensure!(
                self.fact.owner_identity.is_none()
                    && self.owner.is_none()
                    && selected_owner.is_none(),
                "Selected namespace is missing its original owner allocation"
            );
            self.fact.directory_identity =
                Some(ImpulseFileIdentity::from_native(directory.identity()));
            self.fact.phase = ImpulseVaultCreationPhase::DirectoryPinned;
        } else if let Some(selected) = selected_owner {
            self.adopt_selected_owner(selected, context)?;
        } else if let Some(owner) = &self.owner {
            let identity = directory.group_file_identity(owner)?;
            ensure!(
                self.fact
                    .owner_identity
                    .as_ref()
                    .is_some_and(|id| id.matches(&identity)),
                "Retained namespace owner differs from its admitted allocation"
            );
            verify_owner_prefix(owner, &self.fact.namespace)?;
            self.check_owner_grant()?;
        } else {
            ensure!(
                self.fact.owner_identity.is_some(),
                "Unknown namespace owner identity requires a fresh actual owner selection"
            );
        }
        selected_directory.verify()?;
        self.directory_selection = Some(selected_directory);
        self.cold_unattested_directory = false;
        self.fact.validate()
    }

    #[cfg(not(windows))]
    pub(crate) fn reapprove_namespace(
        &mut self,
        _: SnapshotDirectory,
        _: Option<AuthorizedFile>,
        _: &ImpulseDecodeContext,
    ) -> Result<()> {
        anyhow::bail!("Impulse namespace recovery requires native Windows ownership")
    }

    #[cfg(windows)]
    fn adopt_selected_owner(
        &mut self,
        selected: AuthorizedFile,
        context: &ImpulseDecodeContext,
    ) -> Result<()> {
        let directory = self.directory.as_ref().unwrap();
        ensure!(
            fs::read_dir(directory.path())?.try_fold(0usize, |count, entry| -> Result<usize> {
                let entry = entry?;
                ensure!(
                    entry.file_name() == OWNER,
                    "Creation namespace contains a foreign leaf"
                );
                Ok(count + 1)
            })? == 1,
            "Creation namespace does not contain one exact selected owner"
        );
        // Reject a foreign selection before acquiring a writable descriptor.
        let original = platform::open_source(directory.parent(), OWNER)?;
        let original_identity = platform::identity(&original)?;
        ensure!(
            SelectedOwnerApproval::matches_identity(selected.identity(), &original_identity),
            "Selected owner is not the actual namespace owner allocation"
        );
        if let Some(expected) = &self.fact.owner_identity {
            ensure!(
                expected.matches(&original_identity),
                "Known creation owner was replaced"
            );
        }
        let selected = SelectedOwnerApproval::capture(selected, context)?;
        selected.verify_prefix(&self.fact.namespace)?;
        drop(original);
        self.owner_grant = Some(selected.grant.clone());
        self.owner = Some(directory.open_group_file(OWNER, false)?);
        let identity = directory.group_file_identity(self.owner.as_ref().unwrap())?;
        selected.verify_writer(self.owner.as_ref().unwrap(), &identity, context)?;
        if let Some(expected) = &self.fact.owner_identity {
            ensure!(
                expected.matches(&identity),
                "Reapproval does not identify the original known owner"
            );
        }
        self.fact.directory_identity = Some(ImpulseFileIdentity::from_native(directory.identity()));
        self.fact.owner_identity = Some(ImpulseFileIdentity::from_native(&identity));
        self.fact.phase = ImpulseVaultCreationPhase::OwnerAllocated;
        self.fact.validate()
    }
    #[cfg(all(test, not(windows)))]
    pub(crate) fn reapprove_owner(
        &mut self,
        _: AuthorizedFile,
        _: bool,
        _: &ImpulseDecodeContext,
    ) -> Result<()> {
        anyhow::bail!("Impulse namespace recovery requires Windows")
    }

    pub(crate) fn receipt(&self) -> Result<ImpulseVaultReceipt> {
        self.fact.validate()?;
        ensure!(
            self.fact.phase == ImpulseVaultCreationPhase::Ready,
            "Impulse namespace creation is not ready"
        );
        Ok(ImpulseVaultReceipt {
            namespace: self.fact.namespace.clone(),
            profile_identity: self.fact.profile_identity.clone(),
            directory_identity: self.fact.directory_identity.clone().unwrap(),
            owner_identity: self.fact.owner_identity.clone().unwrap(),
        })
    }
    pub(crate) fn take_vault(&mut self) -> Result<ImpulseVault> {
        let receipt = self.receipt()?;
        self.profile.verify()?;
        self.directory
            .as_ref()
            .context("Creation directory descriptor is missing")?
            .verify()?;
        ensure!(
            self.owner.is_some() && self.owner_locked,
            "Creation owner is not retained and locked"
        );
        Ok(ImpulseVault {
            profile: self.profile.clone(),
            directory: self.directory.take().unwrap(),
            owner: self.owner.take().unwrap(),
            receipt,
            armed: false,
            retirement_limiter: self.limiter.clone(),
            pending: Arc::new(Mutex::new(BTreeMap::new())),
            #[cfg(test)]
            cancel_after_write: None,
        })
    }
}

/// A write approval derives from a real selected read descriptor and an exact
/// complete representation. It cannot be serialized or constructed from an ID.
/// The immutable read descriptor is consumed before writable acquisition; the
/// continuously pinned creator must then reacquire this same object and body.
#[cfg(windows)]
struct SelectedOwnerApproval {
    identity: crate::domain::library::FileIdentity,
    changed: i64,
    length: usize,
    body: [u8; OWNER_BYTES],
    grant: crate::library::authority::FileAuthorization,
}
#[cfg(windows)]
impl SelectedOwnerApproval {
    fn matches_identity(
        selected: &crate::domain::library::FileIdentity,
        actual: &FileIdentity,
    ) -> bool {
        selected.kind == "windows_file_id"
            && selected.volume == format!("{:016x}", actual.device)
            && selected.file_id == actual.object
    }
    fn verify_prefix(&self, namespace: &str) -> Result<()> {
        ensure!(
            self.body[..self.length] == owner_header(namespace)[..self.length],
            "Selected owner body is foreign to the original namespace header"
        );
        self.grant.check()
    }
    fn capture(mut selected: AuthorizedFile, context: &ImpulseDecodeContext) -> Result<Self> {
        context.check()?;
        selected.check()?;
        let snapshot = selected.metadata_snapshot()?;
        ensure!(
            snapshot.metadata.len() <= OWNER_BYTES as u64,
            "Selected owner exceeds its admitted recovery size"
        );
        let crate::library::authority::FileChangeStamp::Windows { change_time_100ns } =
            snapshot.change_stamp
        else {
            anyhow::bail!("Selected owner does not have a native Windows representation stamp");
        };
        let length = snapshot.metadata.len() as usize;
        let mut body = [0; OWNER_BYTES];
        selected.seek(SeekFrom::Start(0))?;
        selected.read_exact(&mut body[..length])?;
        context.check()?;
        ensure!(
            selected.change_stamp()? == snapshot.change_stamp
                && selected.metadata()?.len() == length as u64,
            "Selected owner changed while granting bounded write approval"
        );
        let approval = Self {
            identity: selected.identity().clone(),
            changed: change_time_100ns,
            length,
            body,
            grant: selected.authorization(),
        };
        drop(selected);
        approval.grant.check()?;
        Ok(approval)
    }
    fn verify_writer(
        &self,
        writer: &File,
        actual: &FileIdentity,
        context: &ImpulseDecodeContext,
    ) -> Result<()> {
        context.check()?;
        self.grant.check_retained_file(writer)?;
        ensure!(
            Self::matches_identity(&self.identity, actual),
            "Writable owner differs from the actual selected descriptor"
        );
        let stamp = platform::stamp(writer)?;
        ensure!(
            stamp.length == self.length as u64 && stamp.changed == self.changed as i128,
            "Writable owner representation changed after selected descriptor consumption"
        );
        let mut body = [0; OWNER_BYTES];
        read_exact_at(writer, 0, &mut body[..self.length])?;
        ensure!(
            body[..self.length] == self.body[..self.length] && platform::stamp(writer)? == stamp,
            "Writable owner body differs from the complete actual selection"
        );
        self.grant.check_retained_file(writer)?;
        context.check()
    }
}

#[cfg(windows)]
fn verify_owner_prefix(owner: &File, namespace: &str) -> Result<()> {
    let length = owner.metadata()?.len();
    ensure!(
        length <= OWNER_BYTES as u64,
        "Namespace owner exceeds its bounded recovery size"
    );
    let mut body = [0; OWNER_BYTES];
    read_exact_at(owner, 0, &mut body[..length as usize])?;
    ensure!(
        body[..length as usize] == owner_header(namespace)[..length as usize],
        "Namespace owner body is foreign to its creation obligation"
    );
    Ok(())
}

#[cfg(all(test, windows))]
mod recovery_tests;
#[cfg(all(test, windows))]
mod retry_tests;

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use std::time::Duration;
    #[test]
    fn consumed_actual_selection_rejects_a_changed_representation_before_header_write() {
        let temp = tempfile::tempdir().unwrap();
        let profile = SnapshotDirectory::pin_profile(temp.path(), 1).unwrap();
        fs::create_dir(temp.path().join(DIRECTORY)).unwrap();
        let path = temp.path().join(DIRECTORY).join(OWNER);
        fs::write(&path, []).unwrap();
        let selected = crate::library::authority::AuthorityRegistry::default()
            .open_selection(&path)
            .unwrap();
        let context = ImpulseDecodeContext::new(Duration::from_secs(10)).unwrap();
        let approval = SelectedOwnerApproval::capture(selected, &context).unwrap();
        fs::write(&path, b"foreign replacement body").unwrap();
        let mut creator = ImpulseVaultCreator::prepare(&profile).unwrap();
        creator.pin_existing_directory().unwrap();
        let directory = creator.directory.as_ref().unwrap();
        let writer = directory.open_group_file(OWNER, false).unwrap();
        let identity = directory.group_file_identity(&writer).unwrap();
        assert!(
            approval
                .verify_writer(&writer, &identity, &context)
                .is_err()
        );
        assert_eq!(fs::read(&path).unwrap(), b"foreign replacement body");
    }
    #[test]
    fn unknown_owner_fid_needs_fresh_selected_handle_and_write_approval() {
        let temp = tempfile::tempdir().unwrap();
        let profile = SnapshotDirectory::pin_profile(temp.path(), 1).unwrap();
        let mut creator = ImpulseVaultCreator::prepare(&profile).unwrap();
        let context = ImpulseDecodeContext::new(Duration::from_secs(10)).unwrap();
        let mut durable = None;
        assert!(
            creator
                .advance(&context, &mut |fact| {
                    if fact.phase == ImpulseVaultCreationPhase::OwnerAllocated {
                        anyhow::bail!("Injected failed FID ACK");
                    }
                    durable = Some(fact.clone());
                    Ok(())
                })
                .is_err()
        );
        drop(creator);
        let mut resumed = ImpulseVaultCreator::resume(&profile, &durable.unwrap()).unwrap();
        assert!(resumed.advance(&context, &mut |_| Ok(())).is_err());
        let path = temp.path().join(DIRECTORY).join(OWNER);
        let selected = crate::library::authority::AuthorityRegistry::default()
            .open_selection(&path)
            .unwrap();
        assert!(resumed.reapprove_owner(selected, false, &context).is_err());
        let selected = crate::library::authority::AuthorityRegistry::default()
            .open_selection(&path)
            .unwrap();
        resumed.reapprove_owner(selected, true, &context).unwrap();
        resumed.advance(&context, &mut |_| Ok(())).unwrap();
        assert_eq!(
            resumed.receipt().unwrap().namespace,
            resumed.fact().namespace
        );
    }
    #[test]
    fn preallocation_journal_failure_creates_nothing() {
        let temp = tempfile::tempdir().unwrap();
        let profile = SnapshotDirectory::pin_profile(temp.path(), 1).unwrap();
        let mut creator = ImpulseVaultCreator::prepare(&profile).unwrap();
        assert!(
            creator
                .advance(
                    &ImpulseDecodeContext::new(Duration::from_secs(10)).unwrap(),
                    &mut |_| anyhow::bail!("Injected intent persistence failure")
                )
                .is_err()
        );
        assert!(!temp.path().join(DIRECTORY).exists());
    }
    #[test]
    fn failed_owner_inventory_retains_same_fid_and_retry_completes_that_original() {
        let temp = tempfile::tempdir().unwrap();
        let profile = SnapshotDirectory::pin_profile(temp.path(), 1).unwrap();
        let mut creator = ImpulseVaultCreator::prepare(&profile).unwrap();
        let context = ImpulseDecodeContext::new(Duration::from_secs(10)).unwrap();
        assert!(
            creator
                .advance(&context, &mut |fact| {
                    if fact.phase == ImpulseVaultCreationPhase::OwnerAllocated {
                        anyhow::bail!("Injected owner persistence failure");
                    }
                    Ok(())
                })
                .is_err()
        );
        let original = platform::identity(creator.owner.as_ref().unwrap()).unwrap();
        assert_eq!(creator.owner.as_ref().unwrap().metadata().unwrap().len(), 0);
        creator.advance(&context, &mut |_| Ok(())).unwrap();
        let receipt = creator.receipt().unwrap();
        assert!(receipt.owner_identity.matches(&original));
        let mut vault = creator.take_vault().unwrap();
        vault.arm(&receipt).unwrap();
        vault.verify().unwrap();
    }
    #[test]
    fn cold_ready_receipt_recovers_before_the_final_store_acknowledgement() {
        let temp = tempfile::tempdir().unwrap();
        let profile = SnapshotDirectory::pin_profile(temp.path(), 1).unwrap();
        let mut creator = ImpulseVaultCreator::prepare(&profile).unwrap();
        creator
            .advance(
                &ImpulseDecodeContext::new(Duration::from_secs(10)).unwrap(),
                &mut |_| Ok(()),
            )
            .unwrap();
        let fact = creator.fact().clone();
        drop(creator);
        let mut restarted = ImpulseVaultCreator::resume(&profile, &fact).unwrap();
        restarted
            .advance(
                &ImpulseDecodeContext::new(Duration::from_secs(10)).unwrap(),
                &mut |_| Ok(()),
            )
            .unwrap();
        assert_eq!(
            restarted.receipt().unwrap().owner_identity,
            fact.owner_identity.unwrap()
        );
    }
}
