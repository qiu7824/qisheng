use anyhow::{Result, ensure};
use flate2::{Compression, bufread::MultiGzDecoder, write::GzEncoder};
use serde::Serialize;
use serde_json::Value;
use std::io::{self, Read, Write};

pub const MAX_BACKUP_BYTES: usize = 16 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackupEncoding {
    Json,
    Gzip,
    Auto,
}

struct LimitedBuffer(Vec<u8>);
impl Write for LimitedBuffer {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > MAX_BACKUP_BYTES.saturating_sub(self.0.len()) {
            return Err(io::Error::other("Backup exceeds the 16 MiB limit"));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub(super) fn json_bytes<T: Serialize + ?Sized>(value: &T) -> Result<Vec<u8>> {
    let mut writer = LimitedBuffer(Vec::new());
    serde_json::to_writer(&mut writer, value)?;
    Ok(writer.0)
}

pub(super) fn decode(bytes: &[u8], encoding: BackupEncoding) -> Result<Value> {
    ensure!(
        bytes.len() <= MAX_BACKUP_BYTES,
        "Backup input exceeds 16 MiB"
    );
    let gzip = match encoding {
        BackupEncoding::Auto => bytes.starts_with(&[0x1f, 0x8b]),
        BackupEncoding::Gzip => true,
        BackupEncoding::Json => false,
    };
    let decoded;
    let text = if gzip {
        let mut reader = MultiGzDecoder::new(bytes).take((MAX_BACKUP_BYTES + 1) as u64);
        decoded = {
            let mut buffer = Vec::new();
            let mut chunk = [0_u8; 8192];
            loop {
                let read = reader.read(&mut chunk)?;
                if read == 0 {
                    break;
                }
                ensure!(
                    read <= MAX_BACKUP_BYTES.saturating_sub(buffer.len()),
                    "Decompressed backup exceeds 16 MiB"
                );
                buffer.extend_from_slice(&chunk[..read]);
            }
            buffer
        };
        decoded.as_slice()
    } else {
        bytes
    };
    let text = text.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(text);
    let mut value: Value = serde_json::from_slice(text)?;
    // LX 1.14.0 accidentally serialized the complete document twice.
    if let Value::String(inner) = value {
        ensure!(
            inner.len() <= MAX_BACKUP_BYTES,
            "Nested backup exceeds 16 MiB"
        );
        value = serde_json::from_str(&inner)?;
    }
    ensure!(value.is_object(), "Backup must contain a JSON object");
    Ok(value)
}

pub(super) fn encode<T: Serialize + ?Sized>(
    value: &T,
    encoding: BackupEncoding,
) -> Result<Vec<u8>> {
    let json = json_bytes(value)?;
    match encoding {
        BackupEncoding::Json => Ok(json),
        BackupEncoding::Gzip => {
            let mut encoder = GzEncoder::new(LimitedBuffer(Vec::new()), Compression::default());
            encoder.write_all(&json)?;
            Ok(encoder.finish()?.0)
        }
        BackupEncoding::Auto => anyhow::bail!("Export requires an explicit JSON or gzip encoding"),
    }
}
