//! LX v4 protocol data does not confer source, network, or filesystem authority.
mod codec;
mod crypto;
mod merge;
mod model;
pub mod network;
pub(crate) use codec::js_json as canonical_json;

pub use codec::{RpcFrame, decode_message, encode_message, parse_frame, serialize_frame};
pub use crypto::{
    PairingKeyPair, SharedKey, aes_decrypt, aes_encrypt, code_key, generate_pairing_key_pair,
    rsa_decrypt, rsa_encrypt,
};
pub use merge::{
    FirstSyncPlan, dislike_digest, first_sync_dislike, first_sync_lists, merge_dislike_three_way,
    merge_lists_three_way,
};
pub use model::{
    AddLocation, Dataset, DislikeAction, ListAction, ListData, MusicInfo, MusicKind,
    PeerCapabilities, SyncError, SyncErrorKind, SyncMode, SyncResult, UserList,
};

pub const HELLO: &str = "Hello~::^-^::~v4~";
pub const ID_PREFIX: &str = "OjppZDo6";
pub const AUTH_PREFIX: &str = "lx-music auth::";
pub const CONNECT_MESSAGE: &str = "lx-music connect";
pub const MAX_JSON_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_WIRE_BYTES: usize = 12 * 1024 * 1024;
