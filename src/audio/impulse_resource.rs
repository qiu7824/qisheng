//! Reads a selected IR through its actual authorized file object.
use super::impulse_decode::{
    ImpulseDecodeContext, ImpulseDecodeReport, ImpulseEncodedInput, MAX_IMPULSE_ENCODED_BYTES,
    decode_impulse,
};
use crate::{domain::library::FileIdentity, library::authority::AuthorizedFile};
use anyhow::{Result, ensure};
use sha2::{Digest, Sha256};
use std::io::{Read, Seek, SeekFrom};

pub struct PreparedImpulseResource {
    pub decoded: ImpulseDecodeReport,
    pub encoded_sha256: String,
    pub encoded_bytes: u64,
    pub selected_identity: FileIdentity,
}
pub fn prepare_selected_impulse(
    selected: &AuthorizedFile,
    extension: &str,
    context: &ImpulseDecodeContext,
) -> Result<PreparedImpulseResource> {
    context.check()?;
    let original = selected.metadata_snapshot()?;
    let mut file = selected
        .authorization()
        .with_cancel(context.cancel_flag())?
        .open()?;
    let initial = file.metadata_snapshot()?;
    ensure!(
        file.identity() == selected.identity() && initial.change_stamp == original.change_stamp,
        "Impulse selection no longer identifies the same representation"
    );
    let length = initial.metadata.len();
    ensure!(
        length > 0 && length <= MAX_IMPULSE_ENCODED_BYTES as u64,
        "Selected impulse size exceeds its admitted budget"
    );
    let identity = file.identity().clone();
    let mut input = ImpulseEncodedInput::prepare(length as usize)?;
    let mut hash = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    let mut read_bytes = 0u64;
    file.seek(SeekFrom::Start(0))?;
    loop {
        context.check()?;
        ensure!(
            selected.change_stamp()? == original.change_stamp,
            "Original impulse selection was retired or changed"
        );
        ensure!(
            file.identity() == &identity && file.change_stamp()? == initial.change_stamp,
            "Selected impulse changed during reading"
        );
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        input.append(&buffer[..count])?;
        hash.update(&buffer[..count]);
        read_bytes += count as u64;
        context.check()?;
        ensure!(
            file.change_stamp()? == initial.change_stamp,
            "Selected impulse body changed during reading"
        );
    }
    ensure!(
        read_bytes == length
            && file.metadata()?.len() == length
            && file.change_stamp()? == initial.change_stamp,
        "Selected impulse representation changed"
    );
    let digest = format!("{:x}", hash.finalize());
    let decoded = decode_impulse(input, extension, context)?;
    context.check()?;
    ensure!(
        selected.change_stamp()? == original.change_stamp,
        "Original impulse selection was retired during decode"
    );
    ensure!(
        file.identity() == &identity && file.change_stamp()? == initial.change_stamp,
        "Selected impulse changed before preparation completed"
    );
    file.seek(SeekFrom::Start(0))?;
    let mut confirmation = Sha256::new();
    let mut verified_bytes = 0u64;
    loop {
        context.check()?;
        ensure!(
            selected.change_stamp()? == original.change_stamp
                && file.change_stamp()? == initial.change_stamp,
            "Impulse representation changed during final verification"
        );
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        ensure!(
            count as u64 <= length - verified_bytes,
            "Impulse length increased during final verification"
        );
        confirmation.update(&buffer[..count]);
        verified_bytes += count as u64;
    }
    ensure!(
        verified_bytes == length && format!("{:x}", confirmation.finalize()) == digest,
        "Impulse body differs from the decoded representation"
    );
    context.check()?;
    Ok(PreparedImpulseResource {
        decoded,
        encoded_sha256: digest,
        encoded_bytes: length,
        selected_identity: identity,
    })
}
