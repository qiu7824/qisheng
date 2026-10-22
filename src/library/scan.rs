//! Bounded discovery within an explicitly selected directory.
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant, UNIX_EPOCH},
};

const MAX_FILES: usize = 50_000;
const MAX_ENTRIES: usize = 250_000;
const MAX_DIRECTORIES: usize = 10_000;
const MAX_DEPTH: usize = 64;
pub const MAX_SCAN_REPORT_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_SCAN_DIRECTORY_BYTES: usize = 8 * 1024 * 1024;
const MAX_WARNINGS: usize = 256;
#[derive(Clone, Debug)]
pub struct ScanRequest {
    pub root: PathBuf,
    pub recursive: bool,
    pub max_files: usize,
    pub max_entries: usize,
    pub max_depth: usize,
}
impl ScanRequest {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            recursive: true,
            max_files: MAX_FILES,
            max_entries: MAX_ENTRIES,
            max_depth: MAX_DEPTH,
        }
    }
    fn validate(&self) -> Result<()> {
        ensure!(
            self.root.is_absolute(),
            "Library root must be an absolute path"
        );
        ensure!(
            (1..=MAX_FILES).contains(&self.max_files),
            "Library scan file budget is invalid"
        );
        ensure!(
            (1..=MAX_ENTRIES).contains(&self.max_entries),
            "Library scan entry budget is invalid"
        );
        ensure!(self.max_depth <= MAX_DEPTH, "Library scan depth exceeds 64");
        Ok(())
    }
}
#[derive(Clone)]
pub struct ScanContext {
    pub cancel: Arc<AtomicBool>,
    pub deadline: Instant,
}
impl ScanContext {
    pub fn new(timeout: Duration) -> Self {
        let now = Instant::now();
        Self {
            cancel: Arc::new(AtomicBool::new(false)),
            deadline: now.checked_add(timeout).unwrap_or(now),
        }
    }
    pub fn check(&self) -> Result<()> {
        if self.cancel.load(Ordering::Acquire) {
            return Err(ScanError::Cancelled.into());
        }
        if Instant::now() >= self.deadline {
            return Err(ScanError::DeadlineExceeded.into());
        }
        Ok(())
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScanError {
    Cancelled,
    DeadlineExceeded,
    BudgetExceeded,
    UnsupportedPath,
}
impl std::fmt::Display for ScanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Cancelled => "Library scan cancelled",
            Self::DeadlineExceeded => "Library scan deadline exceeded",
            Self::BudgetExceeded => "Library scan budget exceeded",
            Self::UnsupportedPath => "Library scan path cannot be represented in UTF-8",
        })
    }
}
impl std::error::Error for ScanError {}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct DiscoveredFile {
    pub path: PathBuf,
    pub size_bytes: u64,
    pub modified_ms: Option<u64>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ScanWarning {
    pub path: PathBuf,
    pub reason: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ScanReport {
    pub root: PathBuf,
    pub files: Vec<DiscoveredFile>,
    #[serde(default)]
    pub discovered_files: usize,
    pub warnings: Vec<ScanWarning>,
    pub omitted_warnings: usize,
    pub directories_visited: usize,
    pub entries_examined: usize,
    pub complete: bool,
}
fn is_link(metadata: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    {
        metadata.file_type().is_symlink()
    }
}
fn audio_file(path: &Path) -> bool {
    path.extension().and_then(|s| s.to_str()).is_some_and(|s| {
        matches!(
            s.to_ascii_lowercase().as_str(),
            "wav" | "mp3" | "flac" | "ogg" | "oga" | "m4a" | "mp4" | "aac"
        )
    })
}
fn reserve_bytes(bytes: &mut usize, additional: usize) -> Result<()> {
    let total = bytes
        .checked_add(additional)
        .ok_or(ScanError::BudgetExceeded)?;
    if total > MAX_SCAN_REPORT_BYTES {
        return Err(ScanError::BudgetExceeded.into());
    }
    *bytes = total;
    Ok(())
}
impl ScanReport {
    fn warning(
        &mut self,
        path: PathBuf,
        reason: impl Into<String>,
        incomplete: bool,
        bytes: &mut usize,
    ) -> Result<()> {
        if incomplete {
            self.complete = false;
        }
        if self.warnings.len() < MAX_WARNINGS {
            ensure!(path.to_str().is_some(), ScanError::UnsupportedPath);
            let reason = reason.into().chars().take(512).collect();
            let warning = ScanWarning { path, reason };
            reserve_bytes(bytes, serde_json::to_vec(&warning)?.len() + 1)?;
            self.warnings.push(warning);
        } else {
            self.omitted_warnings += 1;
        }
        Ok(())
    }
}
/// Discovers metadata only. It neither decodes audio nor changes any repository.
/// Nested symlinks and Windows reparse points are excluded from the grant.
/// Cancellation is cooperative between synchronous filesystem operations.
pub fn discover(request: &ScanRequest, context: &ScanContext) -> Result<ScanReport> {
    let mut files = Vec::new();
    let mut stored_bytes = 0usize;
    let mut report = discover_stream(request, context, |file| {
        reserve_bytes(
            &mut stored_bytes,
            file.path.capacity() + std::mem::size_of::<DiscoveredFile>() * 2,
        )?;
        files.push(file);
        Ok(())
    })?;
    files.sort_by(|left, right| left.path.cmp(&right.path));
    report.files = files;
    Ok(report)
}

/// Enumerates each directory incrementally. The receiver may send one bounded
/// batch and wait for its acknowledgement before accepting another file.
/// The returned summary does not retain discovered file paths.
pub fn discover_stream(
    request: &ScanRequest,
    context: &ScanContext,
    receive: impl FnMut(DiscoveredFile) -> Result<()>,
) -> Result<ScanReport> {
    discover_impl(request, context, None, receive)
}

pub fn discover_authorized_stream(
    authority: &super::authority::RootAuthority,
    request: &ScanRequest,
    context: &ScanContext,
    receive: impl FnMut(DiscoveredFile) -> Result<()>,
) -> Result<ScanReport> {
    discover_impl(request, context, Some(authority), receive)
}

fn discover_impl(
    request: &ScanRequest,
    context: &ScanContext,
    authority: Option<&super::authority::RootAuthority>,
    mut receive: impl FnMut(DiscoveredFile) -> Result<()>,
) -> Result<ScanReport> {
    request.validate()?;
    context.check()?;
    let root = fs::canonicalize(&request.root).context("Cannot identify selected library root")?;
    ensure!(root.is_dir(), "Selected library root is not a directory");
    ensure!(root.to_str().is_some(), ScanError::UnsupportedPath);
    let mut report = ScanReport {
        root: root.clone(),
        files: Vec::new(),
        discovered_files: 0,
        warnings: Vec::new(),
        omitted_warnings: 0,
        directories_visited: 0,
        entries_examined: 0,
        complete: true,
    };
    let mut pending = vec![(root.clone(), 0usize)];
    let mut directory_bytes = directory_storage_bytes(&root)?;
    let mut reported_bytes = serde_json::to_vec(&report)?.len() + 64;
    let mut visited = HashSet::new();
    while let Some((directory, depth)) = pending.pop() {
        directory_bytes = directory_bytes.saturating_sub(directory_storage_bytes(&directory)?);
        context.check()?;
        let _guard = if let Some(authority) = authority {
            match authority.open_directory(&directory) {
                Ok(guard) => Some(guard),
                Err(error) if depth == 0 => return Err(error),
                Err(error) => {
                    report.warning(directory, error.to_string(), true, &mut reported_bytes)?;
                    continue;
                }
            }
        } else {
            None
        };
        if report.directories_visited >= MAX_DIRECTORIES {
            return Err(ScanError::BudgetExceeded.into());
        }
        let metadata = match fs::symlink_metadata(&directory) {
            Ok(metadata) => metadata,
            Err(error) => {
                report.warning(directory, error.to_string(), true, &mut reported_bytes)?;
                continue;
            }
        };
        if is_link(&metadata) {
            report.warning(
                directory,
                "Directory changed into a link",
                true,
                &mut reported_bytes,
            )?;
            continue;
        }
        let actual = match fs::canonicalize(&directory) {
            Ok(actual) => actual,
            Err(error) => {
                report.warning(directory, error.to_string(), true, &mut reported_bytes)?;
                continue;
            }
        };
        if !actual.starts_with(&root) {
            report.warning(
                directory,
                "Directory is outside selected root",
                true,
                &mut reported_bytes,
            )?;
            continue;
        }
        if visited.contains(&actual) {
            report.warning(
                directory,
                "Directory was already discovered",
                true,
                &mut reported_bytes,
            )?;
            continue;
        }
        reserve_directory_bytes(&mut directory_bytes, directory_storage_bytes(&actual)?)?;
        visited.insert(actual.clone());
        let children: Box<dyn Iterator<Item = std::io::Result<fs::DirEntry>>> =
            match _guard.as_ref().map_or_else(
                || {
                    fs::read_dir(&actual)
                        .map(|entries| {
                            Box::new(entries)
                                as Box<dyn Iterator<Item = std::io::Result<fs::DirEntry>>>
                        })
                        .map_err(anyhow::Error::from)
                },
                |guard| {
                    guard.read_dir().map(|entries| {
                        Box::new(entries) as Box<dyn Iterator<Item = std::io::Result<fs::DirEntry>>>
                    })
                },
            ) {
                Ok(entries) => entries,
                Err(error) => {
                    if depth == 0 {
                        return Err(error).context("Cannot enumerate selected library root");
                    }
                    report.warning(directory, error.to_string(), true, &mut reported_bytes)?;
                    continue;
                }
            };
        report.directories_visited += 1;
        let directory_stamp = _guard
            .as_ref()
            .and_then(|guard| guard.metadata().ok())
            .and_then(|metadata| metadata.modified().ok());
        for child in children {
            context.check()?;
            if report.entries_examined >= request.max_entries {
                return Err(ScanError::BudgetExceeded.into());
            }
            report.entries_examined += 1;
            let child = match child {
                Ok(child) => child,
                Err(error) => {
                    report.warning(actual.clone(), error.to_string(), true, &mut reported_bytes)?;
                    continue;
                }
            };
            context.check()?;
            let path = actual.join(child.file_name());
            let metadata = match fs::symlink_metadata(child.path()) {
                Ok(value) => value,
                Err(error) => {
                    report.warning(path, error.to_string(), true, &mut reported_bytes)?;
                    continue;
                }
            };
            if is_link(&metadata) {
                report.warning(
                    path,
                    "Nested link excluded from selected root",
                    authority.is_some(),
                    &mut reported_bytes,
                )?;
                continue;
            }
            if metadata.is_dir() {
                if request.recursive {
                    if depth >= request.max_depth {
                        report.warning(
                            path,
                            "Directory depth limit reached",
                            true,
                            &mut reported_bytes,
                        )?;
                    } else {
                        if pending.len() + report.directories_visited >= MAX_DIRECTORIES {
                            return Err(ScanError::BudgetExceeded.into());
                        }
                        reserve_directory_bytes(
                            &mut directory_bytes,
                            directory_storage_bytes(&path)?,
                        )?;
                        pending.push((path, depth + 1));
                    }
                }
                continue;
            }
            if !metadata.is_file() || !audio_file(&path) {
                continue;
            }
            let canonical = match fs::canonicalize(&path) {
                Ok(value) => value,
                Err(error) => {
                    report.warning(path, error.to_string(), true, &mut reported_bytes)?;
                    continue;
                }
            };
            if !canonical.starts_with(&root) {
                report.warning(
                    path,
                    "File is outside selected root",
                    true,
                    &mut reported_bytes,
                )?;
                continue;
            }
            if report.discovered_files >= request.max_files {
                return Err(ScanError::BudgetExceeded.into());
            }
            ensure!(canonical.to_str().is_some(), ScanError::UnsupportedPath);
            let modified_ms = metadata
                .modified()
                .ok()
                .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                .and_then(|time| u64::try_from(time.as_millis()).ok());
            let file = DiscoveredFile {
                path: canonical,
                size_bytes: metadata.len(),
                modified_ms,
            };
            reserve_bytes(&mut reported_bytes, serde_json::to_vec(&file)?.len() + 1)?;
            report.discovered_files += 1;
            receive(file)?;
            context.check()?;
        }
        if let Some(guard) = &_guard {
            let after = guard.metadata()?.modified().ok();
            if directory_stamp.is_none() || directory_stamp != after {
                report.warning(
                    actual,
                    "Directory changed during enumeration",
                    true,
                    &mut reported_bytes,
                )?;
            }
        }
    }
    context.check()?;
    if let Some(authority) = authority {
        authority.verify()?;
    }
    Ok(report)
}

fn directory_storage_bytes(path: &PathBuf) -> Result<usize> {
    // The allowance covers the owned OS string, Vec/HashSet spare entries and
    // hash buckets independently of the serialized report budget.
    path.capacity()
        .checked_add(256)
        .ok_or_else(|| ScanError::BudgetExceeded.into())
}
fn reserve_directory_bytes(bytes: &mut usize, additional: usize) -> Result<()> {
    let total = bytes
        .checked_add(additional)
        .ok_or(ScanError::BudgetExceeded)?;
    ensure!(total <= MAX_SCAN_DIRECTORY_BYTES, ScanError::BudgetExceeded);
    *bytes = total;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn warning_json_escapes_share_the_report_budget_and_failure_keeps_warning_out() {
        let mut report = ScanReport {
            root: PathBuf::from("root"),
            files: Vec::new(),
            discovered_files: 0,
            warnings: Vec::new(),
            omitted_warnings: 0,
            directories_visited: 0,
            entries_examined: 0,
            complete: true,
        };
        let mut bytes = MAX_SCAN_REPORT_BYTES - 4;
        assert!(
            report
                .warning(PathBuf::from("\"quoted\""), "line\n", false, &mut bytes)
                .is_err()
        );
        assert!(report.warnings.is_empty());
        assert_eq!(bytes, MAX_SCAN_REPORT_BYTES - 4);
    }
}
