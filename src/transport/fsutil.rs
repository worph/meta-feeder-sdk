//! Filesystem helpers shared by the hull and the plugins that own a corner of
//! the shared `/data` volume: free space, on-disk size, and the grace-period
//! removal both `reclaim` passes (cache containers in the hull, `bt-temp/` in
//! the torrent plugin) are built on. Moved verbatim from meta-share
//! `reclaim.rs` / `eviction.rs`.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use tracing::warn;

/// Free space (bytes available to an unprivileged process) on the filesystem
/// backing `path`; `None` when the query fails or the platform isn't supported.
#[cfg(unix)]
pub fn available_bytes(path: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    // SAFETY: `c_path` is a valid NUL-terminated path; `statvfs` only writes
    // into the zeroed-out struct and returns 0 on success.
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::statvfs(c_path.as_ptr(), &mut stat) };
    if rc != 0 {
        return None;
    }
    #[allow(clippy::unnecessary_cast)]
    Some((stat.f_bavail as u64).saturating_mul(stat.f_frsize as u64))
}

#[cfg(not(unix))]
pub fn available_bytes(_path: &Path) -> Option<u64> {
    None
}

/// Apparent bytes under `dir` (file lengths, subdirectories walked, symlinks
/// skipped). `0` when absent. Blocking.
pub fn dir_size_blocking(dir: &Path) -> u64 {
    let mut total = 0u64;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for entry in rd.flatten() {
            match entry.file_type() {
                Ok(ft) if ft.is_dir() => stack.push(entry.path()),
                Ok(ft) if ft.is_file() => {
                    if let Ok(meta) = entry.metadata() {
                        total = total.saturating_add(meta.len());
                    }
                }
                _ => {}
            }
        }
    }
    total
}

/// Bytes `path` actually occupies: allocated blocks, not apparent length (a
/// streaming-only torrent's file is sparse and its length is the whole film).
/// Blocking.
pub fn disk_usage_blocking(path: &Path) -> u64 {
    let mut total = 0u64;
    let mut stack = vec![path.to_path_buf()];
    while let Some(p) = stack.pop() {
        let Ok(meta) = std::fs::symlink_metadata(&p) else { continue };
        if meta.is_dir() {
            if let Ok(rd) = std::fs::read_dir(&p) {
                stack.extend(rd.flatten().map(|e| e.path()));
            }
        } else if meta.is_file() {
            total = total.saturating_add(allocated(&meta));
        }
    }
    total
}

#[cfg(unix)]
fn allocated(meta: &std::fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    meta.blocks().saturating_mul(512)
}

#[cfg(not(unix))]
fn allocated(meta: &std::fs::Metadata) -> u64 {
    meta.len()
}

/// Top-level entries of `dir` that `is_owned` doesn't claim.
pub async fn unowned_entries(
    dir: &Path,
    is_owned: impl Fn(&std::ffi::OsStr) -> bool,
) -> Vec<(PathBuf, OsString)> {
    let mut out = Vec::new();
    let Ok(mut rd) = tokio::fs::read_dir(dir).await else {
        return out;
    };
    while let Ok(Some(entry)) = rd.next_entry().await {
        let name = entry.file_name();
        if !is_owned(&name) {
            out.push((entry.path(), name));
        }
    }
    out
}

/// Outcome of [`remove_if_stale`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Removal {
    Removed(u64),
    TooYoung,
    Failed,
}

/// Has nothing under `path` changed for `grace`? Unreadable → `false` (leave it).
pub async fn is_stale(path: &Path, grace: Duration, now: SystemTime) -> bool {
    let p = path.to_path_buf();
    let newest = tokio::task::spawn_blocking(move || newest_change_blocking(&p))
        .await
        .ok()
        .flatten();
    match newest {
        Some(t) => now.duration_since(t).map(|age| age >= grace).unwrap_or(false),
        None => false,
    }
}

/// Remove a file or directory tree once nothing under it changed for `grace`.
/// Returns the bytes it occupied on disk.
pub async fn remove_if_stale(path: &Path, grace: Duration, now: SystemTime) -> Removal {
    if !grace.is_zero() && !is_stale(path, grace, now).await {
        return Removal::TooYoung;
    }
    let p = path.to_path_buf();
    let res = tokio::task::spawn_blocking(move || {
        let bytes = disk_usage_blocking(&p);
        let r = match std::fs::symlink_metadata(&p) {
            Ok(m) if m.is_dir() => std::fs::remove_dir_all(&p),
            Ok(_) => std::fs::remove_file(&p),
            Err(e) => Err(e),
        };
        r.map(|()| bytes)
    })
    .await;
    match res {
        Ok(Ok(b)) => Removal::Removed(b),
        Ok(Err(e)) if e.kind() == std::io::ErrorKind::NotFound => Removal::Removed(0),
        Ok(Err(e)) => {
            warn!(path = %path.display(), error = %e, "reclaim: removal failed");
            Removal::Failed
        }
        Err(_) => Removal::Failed,
    }
}

/// The most recent modification or status change of `path` or anything under
/// it. ctime matters: a container renamed from `tmp/` into `cache/` keeps its
/// old mtime, and only ctime records the move.
pub fn newest_change_blocking(path: &Path) -> Option<SystemTime> {
    let mut newest: Option<SystemTime> = None;
    let mut stack = vec![path.to_path_buf()];
    while let Some(p) = stack.pop() {
        let Ok(meta) = std::fs::symlink_metadata(&p) else {
            if p == path {
                return None;
            }
            continue;
        };
        for t in [meta.modified().ok(), change_time(&meta)].into_iter().flatten() {
            newest = Some(newest.map_or(t, |n| n.max(t)));
        }
        if meta.is_dir() {
            if let Ok(rd) = std::fs::read_dir(&p) {
                stack.extend(rd.flatten().map(|e| e.path()));
            }
        }
    }
    newest
}

#[cfg(unix)]
fn change_time(meta: &std::fs::Metadata) -> Option<SystemTime> {
    use std::os::unix::fs::MetadataExt;
    let secs = u64::try_from(meta.ctime()).ok()?;
    Some(SystemTime::UNIX_EPOCH + Duration::from_secs(secs))
}

#[cfg(not(unix))]
fn change_time(_meta: &std::fs::Metadata) -> Option<SystemTime> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn young_entries_survive_and_zero_grace_removes() {
        let d = std::env::temp_dir().join(format!("fsutil-{}", std::process::id()));
        std::fs::create_dir_all(d.join("a")).unwrap();
        std::fs::write(d.join("a/f"), vec![1u8; 4096]).unwrap();
        let now = SystemTime::now();
        assert_eq!(
            remove_if_stale(&d.join("a"), Duration::from_secs(3600), now).await,
            Removal::TooYoung
        );
        assert!(matches!(
            remove_if_stale(&d.join("a"), Duration::ZERO, now).await,
            Removal::Removed(_)
        ));
        assert!(!d.join("a").exists());
        assert_eq!(remove_if_stale(&d.join("gone"), Duration::ZERO, now).await, Removal::Removed(0));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[tokio::test]
    async fn unowned_lists_only_unclaimed_names() {
        let d = std::env::temp_dir().join(format!("fsutil-own-{}", std::process::id()));
        std::fs::create_dir_all(d.join("keep")).unwrap();
        std::fs::create_dir_all(d.join("drop")).unwrap();
        let got = unowned_entries(&d, |n| n == "keep").await;
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].1, "drop");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn sizes_of_a_missing_dir_are_zero() {
        assert_eq!(dir_size_blocking(Path::new("/definitely/not/here")), 0);
        assert_eq!(disk_usage_blocking(Path::new("/definitely/not/here")), 0);
    }
}
