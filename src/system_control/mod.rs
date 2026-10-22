//! System controls adapt typed requests to the same backend command service.

mod dispatcher;
pub mod instance_relay;
#[cfg(windows)]
mod protocol_registration;
#[cfg(windows)]
mod smtc;
mod supervisor;
mod types;
#[cfg(windows)]
mod windows_owner;

pub use dispatcher::{SystemBackend, SystemDispatcher, SystemInputFence, SystemInputSender};
#[cfg(windows)]
pub use protocol_registration::{
    ProtocolRegistrationApproval, ProtocolRegistrationFailure, ProtocolRegistrationProgress,
    ProtocolRegistrationReceipt, ProtocolValueProgress, register_lxmusic_protocol,
};
pub(crate) use supervisor::ManagedWorker;
pub(crate) use types::SourceArtworkOwner;
pub use types::*;
#[cfg(windows)]
pub use windows_owner::{WindowsOwnerState, WindowsSystemOwner};
