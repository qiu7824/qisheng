//! Bounded, inert backup interchange and transaction previews.

mod codec;
pub mod csv;
pub mod file_export;
pub mod lx;
pub mod native;
pub mod settings;

pub use codec::{BackupEncoding, MAX_BACKUP_BYTES};
pub use lx::{ConflictPolicy, ImportPlan, LxBackup, LxBackupKind, LxList, decode_lx, encode_lx};
pub use native::{
    NativeBackup, NativeBackupOptions, NativeImportPlan, decode_native, encode_native,
};
