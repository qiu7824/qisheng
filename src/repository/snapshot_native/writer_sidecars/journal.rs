use super::*;
/// Native journal writes never invoke SQLite, so a first-WAL creation intent
/// cannot recursively cause the allocation it is supposed to precede.
pub(crate) struct NativeSidecarJournal {
    manifest: Mutex<Option<DurableManifest>>,
}
/// Native startup/close failures keep their real file, manifest and source
/// owners. No pathname or serialized creation fact can populate these slots.
pub(crate) struct WriterStartupFailure {
    failure: Option<NativeFailure>,
}
static RETAINED_STARTUPS: Mutex<Vec<NativeFailure>> = Mutex::new(Vec::new());
pub(crate) const DEFAULT_WRITER_SIDECAR_BYTES: u64 = 256 * 1024 * 1024;
pub(crate) fn reserve_writer_permit() -> Result<SnapshotPermit> {
    static LIMITER: std::sync::OnceLock<SnapshotLimiter> = std::sync::OnceLock::new();
    let limiter = LIMITER.get_or_init(|| {
        SnapshotLimiter::new(4 * MAX_SNAPSHOT_BYTES)
            .expect("constant SQLite writer aggregate is within the existing native limit")
    });
    limiter.reserve_job(
        DEFAULT_WRITER_SIDECAR_BYTES,
        3 * DEFAULT_WRITER_SIDECAR_BYTES + 64 * 1024 * 1024 + MAX_MANIFEST_BYTES,
    )
}
impl WriterStartupFailure {
    pub(crate) fn new(failure: NativeFailure) -> Self {
        Self {
            failure: Some(failure),
        }
    }
    pub(crate) fn message(&self) -> &str {
        self.failure
            .as_ref()
            .map_or("SQLite native startup owners are retired", |failure| {
                failure.message.as_str()
            })
    }
    pub(crate) fn retry(&mut self, context: &SnapshotContext) -> Result<()> {
        let Some(mut failure) = self.failure.take() else {
            return Ok(());
        };
        let result = (|| -> Result<()> {
            if let Some(file) = failure.file.take()
                && let Err(mut retained) = file.cleanup_journaled(context, &|_| Ok(()))
            {
                failure.file = retained.file.take();
                anyhow::bail!("{}", retained.message);
            }
            if let Some(manifest) = failure.manifest.take()
                && let Err(mut retained) = manifest.cleanup(context, &|_| Ok(()))
            {
                failure.manifest = retained.manifest.take();
                anyhow::bail!("{}", retained.message);
            }
            if let Some(source) = failure.source.take()
                && let Err(mut retained) = source.close()
            {
                failure.source = retained.source.take();
                anyhow::bail!("{}", retained.message);
            }
            Ok(())
        })();
        if result.is_err() {
            self.failure = Some(failure);
        }
        result
    }
}
impl Drop for WriterStartupFailure {
    fn drop(&mut self) {
        if let Some(failure) = self.failure.take()
            && (failure.file.is_some() || failure.manifest.is_some() || failure.source.is_some())
        {
            // Actual file/manifest slots retain their shared SnapshotPermit:
            // four jobs, sixteen native file objects per job remain bounded.
            RETAINED_STARTUPS
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .push(failure);
        }
    }
}
pub(crate) fn retry_retained_startups(context: &SnapshotContext) -> Result<usize> {
    let retained = std::mem::take(
        &mut *RETAINED_STARTUPS
            .lock()
            .unwrap_or_else(|error| error.into_inner()),
    );
    let mut first = None;
    let mut remaining = 0;
    for failure in retained {
        let mut owner = WriterStartupFailure::new(failure);
        if let Err(error) = owner.retry(context) {
            remaining += 1;
            first.get_or_insert(error);
        }
        // A failed Drop retains the same native objects and their original job.
    }
    if let Some(error) = first {
        Err(error)
    } else {
        Ok(remaining)
    }
}
impl Drop for NativeSidecarJournal {
    fn drop(&mut self) {
        if let Some(manifest) = self
            .manifest
            .get_mut()
            .unwrap_or_else(|error| error.into_inner())
            .take()
        {
            let owner = WriterStartupFailure::new(manifest.retain_failure(anyhow::anyhow!(
                "SQLite writer journal still has actual native cleanup owners"
            )));
            drop(owner);
        }
    }
}
impl NativeSidecarJournal {
    pub(crate) fn retire(&self, context: &SnapshotContext) -> Result<()> {
        <Self as WriterSidecarJournal>::finish(self, context)
    }
    pub(crate) fn create(
        private: &SnapshotDirectory,
        operation: &str,
        profile: &FileIdentity,
        context: &SnapshotContext,
        permit: &SnapshotPermit,
    ) -> std::result::Result<Self, NativeFailure> {
        let created = serde_json::to_vec(&serde_json::json!({"kind":"sqlite_writer_sidecars","operation_id":operation,"profile":profile}))
            .map_err(|error| NativeFailure::new(error.into(), None))?;
        let mut manifest = DurableManifest::create(private, operation, context, permit)?;
        let result = manifest.append_once(SnapshotPhase::Created, &created, context);
        if let Err(error) = result {
            return Err(manifest.retain_failure(error));
        }
        Ok(Self {
            manifest: Mutex::new(Some(manifest)),
        })
    }
}
impl WriterSidecarJournal for NativeSidecarJournal {
    fn creation(&self, fact: &WriterSidecarCreationFact) -> Result<()> {
        let context = SnapshotContext::new(Duration::from_secs(120))?;
        self.manifest
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .as_mut()
            .context("SQLite sidecar journal already retired")?
            .append_checkpoint(
                &serde_json::to_vec(
                    &serde_json::json!({"kind":"sqlite_sidecar_creation","fact":fact}),
                )?,
                &context,
            )
    }
    fn retirement(&self, fact: &DurableCleanupFact) -> Result<()> {
        let context = SnapshotContext::new(Duration::from_secs(120))?;
        self.manifest
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .as_mut()
            .context("SQLite sidecar journal already retired")?
            .append_checkpoint(
                &serde_json::to_vec(
                    &serde_json::json!({"kind":"sqlite_sidecar_retirement","fact":fact}),
                )?,
                &context,
            )
    }
    fn finish(&self, context: &SnapshotContext) -> Result<()> {
        let mut slot = self
            .manifest
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(manifest) = slot.take()
            && let Err(mut failure) = manifest.cleanup(context, &|_| Ok(()))
        {
            *slot = failure.manifest.take().map(|manifest| *manifest);
            anyhow::bail!("{}", failure.message);
        }
        Ok(())
    }
}
