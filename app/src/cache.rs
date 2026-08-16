//! Cache layout and lifecycle.
//!
//! Artifacts are keyed by source path + mtime + size + the parameters that
//! produced them, so reopening a file is instant and changing the project fps
//! does not hand you a stale proxy. Development involves crashes and these
//! files are large, so cleanup runs on startup as well as on exit.

use anyhow::{anyhow, Context, Result};
use clip_core::Rational;
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// Cached artifacts older than this are removed at startup.
const MAX_AGE: Duration = Duration::from_secs(60 * 60 * 24 * 14);

pub fn cache_root() -> Result<PathBuf> {
    let dirs = directories::ProjectDirs::from("", "", "clip-editor")
        .ok_or_else(|| anyhow!("no home directory"))?;
    Ok(dirs.cache_dir().to_path_buf())
}

/// Identity of a source file for caching purposes.
pub fn source_key(path: &Path, fps: Rational) -> Result<String> {
    let meta = fs::metadata(path).with_context(|| format!("stat {}", path.display()))?;
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut h = Sha256::new();
    h.update(path.to_string_lossy().as_bytes());
    h.update(mtime.to_le_bytes());
    h.update(meta.len().to_le_bytes());
    h.update(format!("fps={}/{}", fps.num, fps.den).as_bytes());
    Ok(crate::hex::encode(&h.finalize())[..16].to_string())
}

/// Per-source cache directory: proxy, PCM, waveform peaks, thumbnail strip.
#[derive(Clone, Debug)]
pub struct SourceCache {
    pub dir: PathBuf,
}

impl SourceCache {
    pub fn open(path: &Path, fps: Rational) -> Result<SourceCache> {
        let dir = cache_root()?.join("sources").join(source_key(path, fps)?);
        fs::create_dir_all(&dir)?;
        fs::create_dir_all(dir.join("thumbs"))?;
        Ok(SourceCache { dir })
    }

    pub fn proxy(&self) -> PathBuf {
        self.dir.join("proxy.mkv")
    }

    pub fn pcm(&self) -> PathBuf {
        self.dir.join("audio.f32")
    }

    pub fn peaks(&self) -> PathBuf {
        self.dir.join("peaks.bin")
    }

    pub fn thumb_pattern(&self) -> PathBuf {
        self.dir.join("thumbs").join("thumb_%05d.jpg")
    }

    pub fn thumbs(&self) -> Vec<PathBuf> {
        let Ok(entries) = fs::read_dir(self.dir.join("thumbs")) else {
            return Vec::new();
        };
        let mut out: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "jpg"))
            .collect();
        out.sort();
        out
    }

    /// A file counts as ready only if it exists and is non-empty — a killed
    /// ffmpeg leaves a zero-byte file behind.
    pub fn has(&self, path: &Path) -> bool {
        fs::metadata(path).map(|m| m.len() > 0).unwrap_or(false)
    }

    pub fn touch(&self) {
        let _ = fs::write(self.dir.join(".used"), b"");
    }
}

/// Scratch space for one export run. Segment intermediates are transient and
/// get removed whether the export succeeds, fails, or is cancelled.
pub struct ExportTemp {
    pub dir: PathBuf,
}

/// Unique scratch directory name. Two exports must never share one — the
/// second would delete the first's segments out from under it.
fn unique_tag() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{}-{nanos:x}-{n}", std::process::id())
}

impl ExportTemp {
    pub fn new() -> Result<ExportTemp> {
        let dir = cache_root()?.join("export").join(unique_tag());
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir)?;
        Ok(ExportTemp { dir })
    }

    pub fn segment(&self, i: usize) -> PathBuf {
        self.dir.join(format!("seg_{i:04}.mkv"))
    }

    pub fn list(&self) -> PathBuf {
        self.dir.join("list.txt")
    }
}

impl Drop for ExportTemp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// Remove stale artifacts. Called at startup and again on exit.
pub fn cleanup() {
    let Ok(root) = cache_root() else { return };
    // Export intermediates are never worth keeping across runs.
    let _ = fs::remove_dir_all(root.join("export"));

    let Ok(entries) = fs::read_dir(root.join("sources")) else {
        return;
    };
    let now = SystemTime::now();
    for entry in entries.flatten() {
        let path = entry.path();
        let age = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| now.duration_since(t).ok())
            .unwrap_or_default();
        if age > MAX_AGE {
            let _ = fs::remove_dir_all(&path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_changes_with_fps_and_is_stable() {
        let tmp = std::env::temp_dir().join("clip-editor-cache-test.bin");
        fs::write(&tmp, b"hello").unwrap();
        let a = source_key(&tmp, Rational::new(30, 1)).unwrap();
        let b = source_key(&tmp, Rational::new(30, 1)).unwrap();
        let c = source_key(&tmp, Rational::new(60, 1)).unwrap();
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_eq!(a.len(), 16);
        let _ = fs::remove_file(&tmp);
    }

    #[test]
    fn key_changes_when_the_file_changes() {
        let tmp = std::env::temp_dir().join("clip-editor-cache-test2.bin");
        fs::write(&tmp, b"one").unwrap();
        let a = source_key(&tmp, Rational::new(30, 1)).unwrap();
        fs::write(&tmp, b"a different length").unwrap();
        let b = source_key(&tmp, Rational::new(30, 1)).unwrap();
        assert_ne!(a, b);
        let _ = fs::remove_file(&tmp);
    }
}
