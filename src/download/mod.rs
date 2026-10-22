//! Independent, bounded downloads with explicit URL and destination authority.

pub mod artifact;
mod authority;
pub mod cleanup;
pub mod lyrics;
mod model;
pub mod planning;
mod pool;
pub mod postprocess;
pub mod postprocess_retry;
mod quota;
pub mod task_model;
mod transfer;
mod verify;

pub(crate) use authority::RetryArtifactRetirement;
pub use authority::{
    CommittedAudio, CommittedSidecar, CreationJournalRequired, DestinationGrant, DownloadPlan,
    OwnedFileIo, OwnedPart, OwnedPartRecord, VerifiedOwnedArtifact, VerifiedOwnedAudio,
    run_download_producer, validate_basename,
};
pub(crate) use authority::{
    DownloadNativeFile, DownloadNativeStatus, DownloadProducerObject, ProducerWorkScope,
    close_read_task_handles_after_join, has_retained_download_objects, prepare_retry_native_file,
    retained_download_profile_objects, retained_read_task_objects_after_join,
    retire_read_task_pins_for_recovery,
};
pub use model::*;
pub use pool::{DownloadHandle, DownloadPool, DownloadPoolStats, DownloadShutdownError};
pub use transfer::{DownloadExecutionLease, TransferJob, TransferOutcome};
pub use verify::{AudioVerification, AudioVerifier, PcmDigestEncoding, SymphoniaVerifier};
