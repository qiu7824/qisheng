use crate::domain::lyric::{LyricBundle, LyricField, MAX_LYRIC_BYTES};
use crate::library::authority::FileAuthorization;
use crate::library::metadata::read_local_metadata;
use anyhow::{Context, Result, bail, ensure};
use base64::Engine;
use encoding_rs::GBK;
use serde::Serialize;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

pub const MAX_SIDECAR_BYTES: usize = 2 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub enum LocalLyricProvenance {
    Sidecar,
    Embedded,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub enum LyricEncoding {
    Utf8,
    Utf16Le,
    Utf16Be,
    Gbk,
}

#[derive(Clone, Debug)]
pub struct LocalLyrics {
    pub bundle: LyricBundle,
    pub provenance: LocalLyricProvenance,
    pub sidecar_path: Option<PathBuf>,
    pub encoding: Option<LyricEncoding>,
    pub warnings: Vec<String>,
}

/// Checks the selected audio and only its exact same-name lyric sidecar.
pub fn read_local_lyrics(path: impl AsRef<Path>) -> Result<Option<LocalLyrics>> {
    let audio_path = std::fs::canonicalize(path.as_ref()).context("无法定位所选音频文件")?;
    ensure!(
        std::fs::metadata(&audio_path)?.is_file(),
        "所选音频不是普通文件"
    );
    let sidecar = audio_path.with_extension("lrc");
    let mut warnings = Vec::new();
    if sidecar.try_exists().context("无法检查同名歌词文件")? {
        match read_sidecar(&audio_path, &sidecar) {
            Ok(lyrics) => return Ok(Some(lyrics)),
            Err(error) => warnings.push(format!("同名LRC无法读取：{error}")),
        }
    }
    let krc = audio_path.with_extension("krc");
    if krc.try_exists()? {
        match read_krc_sidecar(&audio_path, &krc) {
            Ok(mut lyrics) => {
                lyrics.warnings.extend(warnings);
                return Ok(Some(lyrics));
            }
            Err(error) => warnings.push(format!("同名KRC无法读取：{error}")),
        }
    }
    let metadata = read_local_metadata(&audio_path).map_err(|error| {
        if warnings.is_empty() {
            error
        } else {
            error.context(warnings.join("；"))
        }
    })?;
    if metadata.embedded_lyrics.is_none() {
        warnings.extend(
            metadata
                .warnings
                .iter()
                .filter(|warning| warning.starts_with("内嵌歌词无法读取"))
                .cloned(),
        );
        if !warnings.is_empty() {
            bail!("{}", warnings.join("；"));
        }
    }
    warnings.extend(metadata.warnings);
    Ok(metadata.embedded_lyrics.map(|bundle| LocalLyrics {
        bundle,
        provenance: LocalLyricProvenance::Embedded,
        sidecar_path: None,
        encoding: None,
        warnings,
    }))
}

pub fn read_authorized_local_lyrics(
    authorization: &FileAuthorization,
) -> Result<Option<LocalLyrics>> {
    let audio = authorization.open()?;
    let audio_path = audio.path().to_path_buf();
    let mut warnings = Vec::new();
    for extension in ["lrc", "krc"] {
        let file = match authorization.open_sidecar(extension) {
            Ok(file) => file,
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
            {
                continue;
            }
            Err(error) => {
                warnings.push(format!(
                    "同名{}无法读取：{error:#}",
                    extension.to_uppercase()
                ));
                continue;
            }
        };
        let result = (|| -> Result<LocalLyrics> {
            let metadata = file.metadata()?;
            ensure!(
                metadata.is_file() && metadata.len() <= MAX_SIDECAR_BYTES as u64,
                "歌词旁车不是普通文件或超过2 MiB限制"
            );
            let sidecar = file.path().to_path_buf();
            let mut bytes = Vec::with_capacity(metadata.len() as usize);
            file.take((MAX_SIDECAR_BYTES + 1) as u64)
                .read_to_end(&mut bytes)?;
            ensure!(bytes.len() <= MAX_SIDECAR_BYTES, "歌词旁车超过2 MiB限制");
            let (bundle, encoding) = if extension == "krc" {
                (super::codec::decode_krc_bytes(&bytes)?, LyricEncoding::Utf8)
            } else {
                let (text, encoding) = decode_lyric_bytes(&bytes)?;
                (decode_lyric_bundle(&text)?, encoding)
            };
            audio.check()?;
            Ok(LocalLyrics {
                bundle,
                provenance: LocalLyricProvenance::Sidecar,
                sidecar_path: Some(sidecar),
                encoding: Some(encoding),
                warnings: warnings.clone(),
            })
        })();
        match result {
            Ok(lyrics) => return Ok(Some(lyrics)),
            Err(error) => warnings.push(format!(
                "同名{}无法读取：{error:#}",
                extension.to_uppercase()
            )),
        }
    }
    let length = audio.metadata()?.len();
    let metadata = crate::library::metadata::read_local_metadata_from_open_file(
        audio.try_clone()?,
        audio_path,
        length,
    )?;
    audio.check()?;
    warnings.extend(metadata.warnings);
    if metadata.embedded_lyrics.is_none() && !warnings.is_empty() {
        bail!("{}", warnings.join("；"));
    }
    Ok(metadata.embedded_lyrics.map(|bundle| LocalLyrics {
        bundle,
        provenance: LocalLyricProvenance::Embedded,
        sidecar_path: None,
        encoding: None,
        warnings,
    }))
}

fn read_krc_sidecar(audio_path: &Path, sidecar: &Path) -> Result<LocalLyrics> {
    let path = std::fs::canonicalize(sidecar).context("无法定位同名KRC文件")?;
    ensure!(
        path.parent() == audio_path.parent(),
        "PermissionRequired: KRC旁车指向所选音频目录之外"
    );
    let file = File::open(&path)?;
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file() && metadata.len() <= MAX_SIDECAR_BYTES as u64,
        "KRC旁车不是普通文件或超过2 MiB限制"
    );
    let mut bytes = Vec::new();
    file.take((MAX_SIDECAR_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    let bundle = super::codec::decode_krc_bytes(&bytes)?;
    Ok(LocalLyrics {
        bundle,
        provenance: LocalLyricProvenance::Sidecar,
        sidecar_path: Some(sidecar.into()),
        encoding: Some(LyricEncoding::Utf8),
        warnings: Vec::new(),
    })
}
fn read_sidecar(audio_path: &Path, sidecar: &Path) -> Result<LocalLyrics> {
    let resolved_sidecar = std::fs::canonicalize(sidecar).context("无法定位同名歌词文件")?;
    ensure!(
        resolved_sidecar.parent() == audio_path.parent(),
        "PermissionRequired: 歌词旁车指向所选音频目录之外"
    );
    let file = File::open(sidecar).context("无法读取同名歌词文件")?;
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file() && metadata.len() <= MAX_SIDECAR_BYTES as u64,
        "歌词旁车不是普通文件或超过2 MiB限制"
    );
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take((MAX_SIDECAR_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= MAX_SIDECAR_BYTES, "歌词旁车超过2 MiB限制");
    let (text, encoding) = decode_lyric_bytes(&bytes)?;
    Ok(LocalLyrics {
        bundle: decode_lyric_bundle(&text)?,
        provenance: LocalLyricProvenance::Sidecar,
        sidecar_path: Some(sidecar.to_owned()),
        encoding: Some(encoding),
        warnings: Vec::new(),
    })
}

pub fn decode_lyric_bytes(bytes: &[u8]) -> Result<(String, LyricEncoding)> {
    ensure!(bytes.len() <= MAX_SIDECAR_BYTES, "歌词旁车超过2 MiB限制");
    let (text, encoding) = if let Some(bytes) = bytes.strip_prefix(&[0xef, 0xbb, 0xbf]) {
        (
            std::str::from_utf8(bytes)
                .context("UTF-8歌词编码损坏")?
                .to_owned(),
            LyricEncoding::Utf8,
        )
    } else if let Some(bytes) = bytes.strip_prefix(&[0xff, 0xfe]) {
        (decode_utf16(bytes, true)?, LyricEncoding::Utf16Le)
    } else if let Some(bytes) = bytes.strip_prefix(&[0xfe, 0xff]) {
        (decode_utf16(bytes, false)?, LyricEncoding::Utf16Be)
    } else if let Some(little_endian) = guess_utf16(bytes) {
        (
            decode_utf16(bytes, little_endian)?,
            if little_endian {
                LyricEncoding::Utf16Le
            } else {
                LyricEncoding::Utf16Be
            },
        )
    } else if let Ok(text) = std::str::from_utf8(bytes) {
        (text.to_owned(), LyricEncoding::Utf8)
    } else {
        let (text, had_errors) = GBK.decode_without_bom_handling(bytes);
        ensure!(!had_errors, "歌词不是有效UTF-8、带BOM的UTF-16或GBK文本");
        (text.into_owned(), LyricEncoding::Gbk)
    };
    ensure!(
        text.len() <= MAX_LYRIC_BYTES && !text.contains('\0'),
        "歌词解码结果超限或含NUL字符"
    );
    Ok((text, encoding))
}

fn guess_utf16(bytes: &[u8]) -> Option<bool> {
    if bytes.len() < 8 || !bytes.len().is_multiple_of(2) {
        return None;
    }
    let pairs: Vec<_> = bytes[..bytes.len().min(16)]
        .as_chunks::<2>()
        .0
        .iter()
        .collect();
    let even_zero = pairs.iter().filter(|pair| pair[0] == 0).count();
    let odd_zero = pairs.iter().filter(|pair| pair[1] == 0).count();
    if odd_zero * 3 >= pairs.len() * 2 && even_zero * 4 < pairs.len() {
        Some(true)
    } else if even_zero * 3 >= pairs.len() * 2 && odd_zero * 4 < pairs.len() {
        Some(false)
    } else {
        None
    }
}

fn decode_utf16(bytes: &[u8], little_endian: bool) -> Result<String> {
    ensure!(bytes.len().is_multiple_of(2), "UTF-16歌词字节数无效");
    let words: Vec<_> = bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|word| {
            if little_endian {
                u16::from_le_bytes([word[0], word[1]])
            } else {
                u16::from_be_bytes([word[0], word[1]])
            }
        })
        .collect();
    String::from_utf16(&words).context("UTF-16歌词含无效代理字符")
}

/// Restores LX's UTF-8/Base64 awlrc container while retaining ordinary lyrics.
pub fn decode_lyric_bundle(text: &str) -> Result<LyricBundle> {
    ensure!(
        text.len() <= MAX_LYRIC_BYTES && !text.contains('\0'),
        "歌词文本超限或含NUL字符"
    );
    let mut bundle = LyricBundle {
        lyric: LyricField::Text(text.trim().to_owned()),
        ..LyricBundle::default()
    };
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim_start();
        if trimmed
            .get(..7)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("[awlrc:"))
        {
            let end = trimmed.find(']').context("LX歌词容器缺少结束标记")?;
            let content = &trimmed[7..end];
            let start = offset + line.len() - trimmed.len();
            let tag_end = start + end + 1;
            bundle.lyric = LyricField::Text(
                format!("{}{}", &text[..start], &text[tag_end..])
                    .trim()
                    .to_owned(),
            );
            for field in content.split(',') {
                let Some((kind, encoded)) = field.trim().split_once(':') else {
                    continue;
                };
                let target = match kind.to_ascii_lowercase().as_str() {
                    "lrc" => &mut bundle.lyric,
                    "tlrc" => &mut bundle.tlyric,
                    "rlrc" => &mut bundle.rlyric,
                    "awlrc" => &mut bundle.lxlyric,
                    _ => continue,
                };
                let encoded = encoded.trim();
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(encoded)
                    .or_else(|_| base64::engine::general_purpose::STANDARD_NO_PAD.decode(encoded))
                    .context("LX歌词容器Base64无效")?;
                let decoded = std::str::from_utf8(&bytes).context("LX歌词容器不是UTF-8文本")?;
                *target = LyricField::Text(decoded.trim().to_owned());
            }
            break;
        }
        offset += line.len();
    }
    bundle.validate()?;
    Ok(bundle)
}
