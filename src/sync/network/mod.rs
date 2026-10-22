//! Owned sync transport; backend tickets are checked at actual repository effects.
mod backend;
mod http;
mod io_guard;
mod memory;
mod owner;
mod protocol;
mod session;

pub use backend::{
    ClientCredentials, PeerRecord, SessionTicket, SyncBackend, SyncBaseline, SyncChanges, SyncData,
    SyncDelta, SyncOperation, SyncReceipt, SyncSnapshot,
};
#[allow(unused_imports)]
pub(crate) use owner::SyncShutdownToken;
pub use owner::{
    ClientConfig, NetworkEvent, NetworkState, NetworkView, PairingCode, PeerEndpoint, ServerConfig,
    SyncNetwork,
};
