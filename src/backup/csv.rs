//! Display-only CSV interchange; rows carry no platform or file authority.

use super::{MAX_BACKUP_BYTES, lx::MAX_IMPORT_TRACKS};
use crate::domain::music::TrackRecord;
use anyhow::{Result, ensure};

const MAX_COLUMNS: usize = 128;
const MAX_CELL_BYTES: usize = 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DisplayCsvTable {
    pub header: Vec<String>,
    pub rows: Vec<Vec<String>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CsvMapping {
    pub name: usize,
    pub singer: Option<usize>,
    pub album: Option<usize>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CsvMusicRow {
    pub name: String,
    pub singer: String,
    pub album: String,
    /// Retains every original column. A later matching preview must obtain an
    /// actual music identity before constructing a repository track.
    pub original_columns: Vec<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum FieldState {
    Start,
    Bare,
    Quoted,
    Closed,
}

pub fn decode_csv(bytes: &[u8]) -> Result<DisplayCsvTable> {
    ensure!(bytes.len() <= MAX_BACKUP_BYTES, "CSV exceeds 16 MiB");
    let text = std::str::from_utf8(bytes)?.trim_start_matches('\u{feff}');
    ensure!(!text.contains('\0'), "CSV contains a NUL character");
    let mut characters = text.chars().peekable();
    let mut state = FieldState::Start;
    let mut field = String::new();
    let mut row = Vec::new();
    let mut records = Vec::new();
    let mut started = false;
    while let Some(character) = characters.next() {
        if state == FieldState::Quoted {
            if character == '"' {
                if characters.peek() == Some(&'"') {
                    characters.next();
                    field.push('"');
                } else {
                    state = FieldState::Closed;
                }
            } else {
                field.push(character);
            }
        } else {
            match character {
                '"' if state == FieldState::Start => {
                    state = FieldState::Quoted;
                    started = true;
                }
                '"' => anyhow::bail!("CSV has a quote in an unquoted field"),
                ',' => {
                    push_field(&mut row, &mut field)?;
                    state = FieldState::Start;
                    started = true;
                }
                '\n' | '\r' => {
                    if character == '\r' && characters.peek() == Some(&'\n') {
                        characters.next();
                    }
                    push_field(&mut row, &mut field)?;
                    push_row(&mut records, &mut row)?;
                    state = FieldState::Start;
                    started = false;
                }
                _ => {
                    ensure!(
                        state != FieldState::Closed,
                        "CSV contains characters after a closing quote"
                    );
                    field.push(character);
                    state = FieldState::Bare;
                    started = true;
                }
            }
        }
        ensure!(field.len() <= MAX_CELL_BYTES, "CSV field exceeds 1 MiB");
    }
    ensure!(
        state != FieldState::Quoted,
        "CSV quoted field is not closed"
    );
    if started || !row.is_empty() || !field.is_empty() {
        push_field(&mut row, &mut field)?;
        push_row(&mut records, &mut row)?;
    }
    ensure!(!records.is_empty(), "CSV header is missing");
    let header = records.remove(0);
    ensure!(
        records.iter().all(|row| row.len() == header.len()),
        "CSV rows have different column counts"
    );
    Ok(DisplayCsvTable {
        header,
        rows: records,
    })
}

fn push_field(row: &mut Vec<String>, field: &mut String) -> Result<()> {
    ensure!(row.len() < MAX_COLUMNS, "CSV has too many columns");
    row.push(std::mem::take(field));
    Ok(())
}
fn push_row(records: &mut Vec<Vec<String>>, row: &mut Vec<String>) -> Result<()> {
    ensure!(records.len() <= MAX_IMPORT_TRACKS, "CSV has too many rows");
    records.push(std::mem::take(row));
    Ok(())
}

impl DisplayCsvTable {
    pub fn map_music(&self, mapping: &CsvMapping) -> Result<Vec<CsvMusicRow>> {
        ensure!(
            self.rows.len() <= MAX_IMPORT_TRACKS && self.header.len() <= MAX_COLUMNS,
            "CSV table is too large"
        );
        let mut indices = vec![mapping.name];
        indices.extend(mapping.singer);
        indices.extend(mapping.album);
        ensure!(
            indices.iter().all(|index| *index < self.header.len()),
            "CSV column mapping is out of range"
        );
        let mut unique = indices.clone();
        unique.sort_unstable();
        unique.dedup();
        ensure!(unique.len() == indices.len(), "CSV mapping reuses a column");
        self.rows
            .iter()
            .map(|row| {
                ensure!(
                    row.len() == self.header.len(),
                    "CSV row has the wrong number of columns"
                );
                let name = row[mapping.name].clone();
                ensure!(
                    !name.is_empty() && name.len() <= MAX_CELL_BYTES && !name.contains('\0'),
                    "CSV music name is invalid"
                );
                Ok(CsvMusicRow {
                    name,
                    singer: mapping
                        .singer
                        .map(|index| row[index].clone())
                        .unwrap_or_default(),
                    album: mapping
                        .album
                        .map(|index| row[index].clone())
                        .unwrap_or_default(),
                    original_columns: row.clone(),
                })
            })
            .collect()
    }

    pub fn encode(&self) -> Result<Vec<u8>> {
        ensure!(
            !self.header.is_empty()
                && self.header.len() <= MAX_COLUMNS
                && self.rows.len() <= MAX_IMPORT_TRACKS,
            "CSV table shape is invalid"
        );
        let mut result = Vec::from([0xef, 0xbb, 0xbf]);
        for row in std::iter::once(&self.header).chain(&self.rows) {
            ensure!(
                row.len() == self.header.len(),
                "CSV row has the wrong number of columns"
            );
            for (index, field) in row.iter().enumerate() {
                if index != 0 {
                    append(&mut result, b",")?;
                }
                append_cell_parts(&mut result, std::iter::once(field.as_str()), "")?;
            }
            append(&mut result, b"\r\n")?;
        }
        Ok(result)
    }
}
fn append_cell_parts<'a, I>(result: &mut Vec<u8>, parts: I, separator: &str) -> Result<()>
where
    I: Iterator<Item = &'a str> + Clone,
{
    let mut bytes = 0usize;
    let mut count = 0usize;
    let mut quoted = false;
    for part in parts.clone() {
        ensure!(!part.contains('\0'), "CSV field is invalid");
        if count != 0 {
            bytes = bytes.saturating_add(separator.len());
        }
        bytes = bytes.saturating_add(part.len());
        ensure!(bytes <= MAX_CELL_BYTES, "CSV field exceeds 1 MiB");
        quoted |= part.contains([',', '"', '\r', '\n']);
        count += 1;
    }
    quoted |= count > 1 && separator.contains([',', '"', '\r', '\n']);
    if quoted {
        append(result, b"\"")?;
    }
    for (index, part) in parts.enumerate() {
        if index != 0 {
            append_escaped(result, separator)?;
        }
        append_escaped(result, part)?;
    }
    if quoted {
        append(result, b"\"")?;
    }
    Ok(())
}
fn append_escaped(result: &mut Vec<u8>, value: &str) -> Result<()> {
    for segment in value.split_inclusive('"') {
        append(result, segment.as_bytes())?;
        if segment.ends_with('"') {
            append(result, b"\"")?;
        }
    }
    Ok(())
}
fn append(result: &mut Vec<u8>, bytes: &[u8]) -> Result<()> {
    ensure!(
        bytes.len() <= MAX_BACKUP_BYTES.saturating_sub(result.len()),
        "CSV exceeds 16 MiB"
    );
    result.extend_from_slice(bytes);
    Ok(())
}

pub fn export_music_csv(tracks: &[TrackRecord], header: [String; 3]) -> Result<Vec<u8>> {
    ensure!(tracks.len() <= MAX_IMPORT_TRACKS, "CSV has too many songs");
    let mut result = Vec::from([0xef, 0xbb, 0xbf]);
    for (index, field) in header.iter().enumerate() {
        if index != 0 {
            append(&mut result, b",")?;
        }
        append_cell_parts(&mut result, std::iter::once(field.as_str()), "")?;
    }
    append(&mut result, b"\r\n")?;
    for track in tracks {
        append_cell_parts(&mut result, std::iter::once(track.title.as_str()), "")?;
        append(&mut result, b",")?;
        append_cell_parts(&mut result, track.artists.iter().map(String::as_str), "、")?;
        append(&mut result, b",")?;
        append_cell_parts(&mut result, std::iter::once(track.album.as_str()), "")?;
        append(&mut result, b"\r\n")?;
    }
    Ok(result)
}
