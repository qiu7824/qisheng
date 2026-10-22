//! Bounded native lyric codecs for the fixed LX Music Desktop provider contracts.
#[path = "codec/qrc.rs"]
mod qrc;

use crate::domain::lyric::{LyricBundle, LyricField, MAX_LYRIC_BYTES};
use anyhow::{Context, Result, ensure};
use base64::Engine;
use flate2::{Decompress, FlushDecompress, Status, read::ZlibDecoder};
use regex::Regex;
use serde_json::Value;
use std::{io::Read, sync::LazyLock};

const KRC_KEY: [u8; 16] = [
    0x40, 0x47, 0x61, 0x77, 0x5e, 0x32, 0x74, 0x47, 0x51, 0x36, 0x31, 0x2d, 0xce, 0xd2, 0x6e, 0x69,
];
static LINE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s*\[(\d+),\d+\]").unwrap());
static WORD_POST: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\((\d+),(\d+)\)").unwrap());
static KRC_WORD: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"<(\d+),(\d+),\d+>").unwrap());
static LX_WORD: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"<-?\d+,\d+>").unwrap());
static LANGUAGE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\[language:([A-Za-z0-9_=+/\\]+)\]").unwrap());
static TIME: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\[([\d:.]+)\]").unwrap());
static ENTITY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"&(#039|amp|lt|gt|quot|apos|nbsp);").unwrap());

fn bound(text: &str) -> Result<()> {
    ensure!(
        text.len() <= MAX_LYRIC_BYTES,
        "Lyric codec input exceeds 2 MiB"
    );
    Ok(())
}
fn timestamp(ms: u64) -> String {
    format!(
        "[{:02}:{:02}.{:03}]",
        ms / 60_000,
        (ms / 1000) % 60,
        ms % 1000
    )
}
fn empty_bundle() -> LyricBundle {
    LyricBundle {
        lyric: LyricField::Text(String::new()),
        tlyric: LyricField::Text(String::new()),
        rlyric: LyricField::Text(String::new()),
        lxlyric: LyricField::Text(String::new()),
    }
}
fn html(text: &str) -> String {
    ENTITY
        .replace_all(text, |cap: &regex::Captures<'_>| match &cap[1] {
            "amp" => "&".into(),
            "lt" => "<".into(),
            "gt" => ">".into(),
            "quot" => "\"".into(),
            "apos" => "'".into(),
            "nbsp" => " ".into(),
            entity => {
                let code = entity
                    .strip_prefix("#x")
                    .and_then(|v| u32::from_str_radix(v, 16).ok())
                    .or_else(|| entity.strip_prefix('#').and_then(|v| v.parse().ok()));
                code.and_then(char::from_u32)
                    .map_or_else(|| cap[0].to_string(), |ch| ch.to_string())
            }
        })
        .into_owned()
}

pub fn decode_krc(input: &str) -> Result<LyricBundle> {
    ensure!(
        input.len() <= MAX_LYRIC_BYTES * 2,
        "Encoded KRC exceeds its limit"
    );
    if input.is_empty() {
        return Ok(empty_bundle());
    }
    let data = base64::engine::general_purpose::STANDARD
        .decode(input)
        .context("Invalid KRC Base64")?;
    decode_krc_bytes(&data)
}
pub fn decode_krc_bytes(input: &[u8]) -> Result<LyricBundle> {
    ensure!(
        input.len() >= 4 && input.len() <= MAX_LYRIC_BYTES,
        "Invalid or oversized KRC container"
    );
    ensure!(&input[..4] == b"krc1", "Invalid KRC header");
    let bytes: Vec<_> = input[4..]
        .iter()
        .enumerate()
        .map(|(i, b)| b ^ KRC_KEY[i % 16])
        .collect();
    let mut decoded = Vec::new();
    ZlibDecoder::new(bytes.as_slice())
        .take((MAX_LYRIC_BYTES + 1) as u64)
        .read_to_end(&mut decoded)
        .context("Invalid KRC compressed data")?;
    ensure!(
        decoded.len() <= MAX_LYRIC_BYTES,
        "KRC expansion exceeds 2 MiB"
    );
    parse_krc_text(std::str::from_utf8(&decoded).context("KRC is not UTF-8")?)
}
pub fn parse_krc_text(input: &str) -> Result<LyricBundle> {
    bound(input)?;
    let mut text = input.replace('\r', "");
    if let Some((first, rest)) = text
        .split_once('\n')
        .filter(|(first, _)| first.contains("[id:$") && first.ends_with(']'))
    {
        let _ = first;
        text = rest.to_owned();
    }
    let mut translated: Option<Vec<Value>> = None;
    let mut romanized: Option<Vec<Value>> = None;
    if let Some(capture) = LANGUAGE.captures(&text) {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(&capture[1])
            .context("Invalid KRC language Base64")?;
        let value: Value = serde_json::from_slice(&bytes).context("Invalid KRC language JSON")?;
        for channel in value
            .get("content")
            .and_then(Value::as_array)
            .context("KRC language content must be an array")?
        {
            let Some(rows) = channel.get("lyricContent").and_then(Value::as_array) else {
                continue;
            };
            match channel.get("type").and_then(Value::as_u64) {
                Some(0) => romanized = Some(rows.clone()),
                Some(1) => translated = Some(rows.clone()),
                _ => {}
            }
        }
        text = LANGUAGE.replace(&text, "").into_owned();
    }
    let (mut lx, mut ordinary, mut trans, mut roma) =
        (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    let mut index = 0;
    for line in text.lines() {
        if let Some(capture) = LINE.captures(line) {
            let start: u64 = capture[1].parse()?;
            let label = timestamp(start);
            let words = &line[capture[0].len()..];
            let words = KRC_WORD.replace_all(words, "<$1,$2>");
            let full = html(&format!("{label}{words}"));
            ordinary.push(LX_WORD.replace_all(&full, "").into_owned());
            lx.push(full);
            let join = |rows: &Vec<Value>| -> String {
                rows.get(index)
                    .and_then(Value::as_array)
                    .map(|cells| cells.iter().filter_map(Value::as_str).collect::<String>())
                    .unwrap_or_default()
            };
            if let Some(rows) = &translated {
                trans.push(html(&format!("{label}{}", join(rows))));
            }
            if let Some(rows) = &romanized {
                roma.push(html(&format!("{label}{}", join(rows))));
            }
            index += 1;
        } else if !line.trim().is_empty() {
            let value = html(line);
            ordinary.push(value.clone());
            lx.push(value);
        }
    }
    let bundle = LyricBundle {
        lyric: LyricField::Text(ordinary.join("\n")),
        tlyric: LyricField::Text(trans.join("\n")),
        rlyric: LyricField::Text(roma.join("\n")),
        lxlyric: LyricField::Text(lx.join("\n")),
    };
    bundle.validate()?;
    Ok(bundle)
}

/// Preserves LX's QRC partial-stream tolerance while bounding both input and output.
pub fn decode_qrc(input: &str) -> Result<String> {
    ensure!(
        input.len() <= MAX_LYRIC_BYTES * 2,
        "QRC hex exceeds its limit"
    );
    if input.is_empty() || !input.len().is_multiple_of(2) {
        return Ok(String::new());
    }
    let mut bytes = Vec::new();
    for pair in input.as_bytes().as_chunks::<2>().0 {
        let Ok(value) = std::str::from_utf8(pair)
            .ok()
            .and_then(|s| u8::from_str_radix(s, 16).ok())
            .ok_or(())
        else {
            break;
        };
        bytes.push(value);
    }
    if bytes.is_empty() {
        return Ok(String::new());
    }
    qrc::decrypt(&mut bytes);
    let mut stream = Decompress::new(true);
    let mut out = Vec::new();
    let mut offset = 0;
    loop {
        let mut chunk = [0u8; 8192];
        let before_in = stream.total_in();
        let before_out = stream.total_out();
        let status = match stream.decompress(&bytes[offset..], &mut chunk, FlushDecompress::Sync) {
            Ok(status) => status,
            Err(_) => return Ok(String::new()),
        };
        let used = (stream.total_in() - before_in) as usize;
        let written = (stream.total_out() - before_out) as usize;
        offset += used;
        ensure!(
            out.len() + written <= MAX_LYRIC_BYTES,
            "QRC expansion exceeds 2 MiB"
        );
        out.extend_from_slice(&chunk[..written]);
        if status == Status::StreamEnd || (used == 0 && written == 0) {
            break;
        }
    }
    Ok(String::from_utf8_lossy(&out).into_owned())
}
fn qrc_content(text: &str) -> Result<String> {
    bound(text)?;
    if !text.contains("LyricContent=") {
        return Ok(text.to_owned());
    }
    let doc = roxmltree::Document::parse_with_options(
        text,
        roxmltree::ParsingOptions {
            allow_dtd: false,
            nodes_limit: 10_000,
            entity_resolver: None,
        },
    )
    .context("Invalid QRC XML")?;
    for node in doc.descendants() {
        if let Some(attribute) = node
            .attributes()
            .find(|attribute| attribute.name() == "LyricContent")
        {
            return Ok(text[attribute.range_value()].to_owned());
        }
    }
    anyhow::bail!("QRC XML lacks LyricContent")
}
fn convert_postfix(text: &str, clamp: bool, retain_plain: bool) -> Result<(String, String)> {
    bound(text)?;
    let (mut plain, mut words) = (Vec::new(), Vec::new());
    for line in text.replace('\r', "").lines() {
        let line = line.trim();
        let Some(capture) = LINE.captures(line) else {
            if line.starts_with("[offset") {
                plain.push(line.into());
                words.push(line.into());
            } else if retain_plain && TIME.is_match(line) {
                plain.push(line.into());
            }
            continue;
        };
        let start: i64 = capture[1].parse()?;
        let label = timestamp(start as u64);
        let content = &line[capture[0].len()..];
        plain.push(format!("{label}{}", WORD_POST.replace_all(content, "")));
        let mut pieces = String::new();
        let mut previous = 0;
        for word in WORD_POST.captures_iter(content) {
            let mark = word.get(0).unwrap();
            let absolute: i64 = word[1].parse()?;
            let duration: u64 = word[2].parse()?;
            let relative = absolute.saturating_sub(start);
            let relative = if clamp { relative.max(0) } else { relative };
            pieces.push_str(&format!(
                "<{relative},{duration}>{}",
                &content[previous..mark.start()]
            ));
            previous = mark.end();
        }
        if !pieces.is_empty() {
            words.push(format!("{label}{pieces}"));
        }
    }
    Ok((plain.join("\n"), words.join("\n")))
}
pub fn qrc_to_lrc(text: &str) -> Result<String> {
    Ok(convert_postfix(&qrc_content(text)?, true, true)?.0)
}
pub fn qrc_to_lx(text: &str) -> Result<String> {
    Ok(convert_postfix(&qrc_content(text)?, true, true)?.1)
}
pub fn mrc_to_lrc(text: &str) -> Result<String> {
    Ok(convert_postfix(text, false, false)?.0)
}
pub fn mrc_to_lx(text: &str) -> Result<String> {
    Ok(convert_postfix(text, false, false)?.1)
}

fn alignment_time(label: &str, pad_fraction: bool) -> Option<i64> {
    let mut parts: Vec<_> = label.split([':', '.']).map(str::to_owned).collect();
    if parts.len() == 2 {
        parts.push("0".into());
    }
    if parts.len() != 3 {
        return None;
    }
    if pad_fraction {
        while parts[2].len() < 3 {
            parts[2].push('0');
        }
    }
    // Upstream getIntv uses this historical minute multiplier in alignment only.
    parts[0]
        .parse::<i64>()
        .ok()?
        .checked_mul(3_600_000)?
        .checked_add(parts[1].parse::<i64>().ok()?.checked_mul(1000)?)?
        .checked_add(parts[2].parse::<i64>().ok()?)
}
fn align_aux(aux: &str, main: &str, pad_fraction: bool) -> String {
    let targets: Vec<_> = main
        .lines()
        .filter_map(|line| {
            TIME.captures(line)
                .map(|capture| (capture[0].to_owned(), alignment_time(&capture[1], false)))
        })
        .collect();
    let mut cursor = 0;
    let mut output = Vec::new();
    for line in aux.lines() {
        let Some(capture) = TIME.captures(line) else {
            continue;
        };
        if line[capture[0].len()..].trim().is_empty() {
            continue;
        }
        let Some(time) = alignment_time(&capture[1], pad_fraction) else {
            continue;
        };
        while let Some((label, target)) = targets.get(cursor) {
            cursor += 1;
            if target.is_some_and(|target| time.abs_diff(target) < 100) {
                output.push(format!("{label}{}", &line[capture[0].len()..]));
                break;
            }
        }
    }
    output.join("\n")
}
pub fn decode_tx_lyrics(
    main: &str,
    translated: Option<&str>,
    romanized: Option<&str>,
) -> Result<LyricBundle> {
    let decoded = decode_qrc(main)?;
    let (plain, words) = convert_postfix(&qrc_content(&decoded)?, true, true)?;
    let translation = translated
        .map(|text| decode_qrc(text).map(|text| align_aux(&text, &plain, true)))
        .transpose()?;
    let romanization = romanized
        .map(|text| {
            decode_qrc(text)
                .and_then(|text| qrc_to_lrc(&text))
                .map(|text| align_aux(&text, &plain, false))
        })
        .transpose()?;
    let bundle = LyricBundle {
        lyric: LyricField::Text(plain),
        lxlyric: LyricField::Text(words),
        tlyric: translation.map_or(LyricField::Missing, LyricField::Text),
        rlyric: romanization.map_or(LyricField::Missing, LyricField::Text),
    };
    bundle.validate()?;
    Ok(bundle)
}

pub fn decode_mrc(input: &str) -> Result<String> {
    bound(input)?;
    if input.len() < 32 {
        return Ok(input.into());
    }
    let mut data = Vec::new();
    for group in input.as_bytes().as_chunks::<16>().0 {
        data.push(
            u64::from_str_radix(std::str::from_utf8(group)?, 16).context("Invalid MRC hex")? as i64,
        );
    }
    let n = data.len();
    let key = [
        27303562373562475i64,
        18014862372307051,
        22799692160172081,
        34058940340699235,
    ];
    let delta = 2654435769i64;
    let mut y = data[0];
    let mut sum = (6 + 52 / n as i64).wrapping_mul(delta);
    while sum != 0 {
        let e = ((sum >> 2) & 3) as usize;
        for p in (1..n).rev() {
            let z = data[p - 1];
            let mix = (y ^ sum).wrapping_add(z ^ key[(p & 3) ^ e])
                ^ ((z >> 5) ^ y.wrapping_shl(2)).wrapping_add((y >> 3) ^ z.wrapping_shl(4));
            data[p] = data[p].wrapping_sub(mix);
            y = data[p];
        }
        let z = data[n - 1];
        let mix = (key[e] ^ z).wrapping_add(y ^ sum)
            ^ ((z >> 5) ^ y.wrapping_shl(2)).wrapping_add((y >> 3) ^ z.wrapping_shl(4));
        data[0] = data[0].wrapping_sub(mix);
        y = data[0];
        sum = sum.wrapping_sub(delta);
    }
    let words: Vec<_> = data
        .iter()
        .flat_map(|value| value.to_le_bytes())
        .collect::<Vec<_>>()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .collect();
    let out = String::from_utf16_lossy(&words);
    bound(&out)?;
    Ok(out)
}
