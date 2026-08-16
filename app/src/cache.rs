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

    fn thumbs_dir(&self) -> PathBuf {
        self.dir.join("thumbs")
    }

    pub fn thumb_pattern(&self) -> PathBuf {
        self.thumbs_dir().join("thumb_%05d.jpg")
    }

    /// Whether the strip on disk is the whole strip.
    ///
    /// "The directory has files in it" is not the same question: a run killed
    /// halfway leaves N of M thumbnails, and treating that as done shows a
    /// truncated strip on every future open of this source. The proxy and PCM
    /// get this right by checking a single file for non-emptiness; a directory
    /// filled one entry at a time needs something written at the end instead.
    pub fn thumbs_complete(&self) -> bool {
        self.thumbs_dir().join(".complete").exists()
    }

    /// Clear the strip and mark it unfinished, before generating it again.
    pub fn begin_thumbs(&self) {
        let dir = self.thumbs_dir();
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::create_dir_all(&dir);
    }

    /// Mark the strip finished. Called only when ffmpeg exited cleanly and was
    /// not cancelled — the marker is the claim that everything is here.
    pub fn finish_thumbs(&self) {
        let _ = fs::write(self.thumbs_dir().join(".complete"), b"");
    }

    pub fn thumbs(&self) -> Vec<PathBuf> {
        let Ok(entries) = fs::read_dir(self.thumbs_dir()) else {
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

    /// Record that this source was opened, so [`cleanup`] keeps it.
    ///
    /// The timestamp is the marker's *contents*, not any filesystem timestamp:
    /// a directory's mtime only moves when an entry is added or removed, not
    /// when a file inside it is rewritten, so it cannot record a re-open.
    /// Writing seconds into the file keeps this and [`last_used`] talking
    /// about the same thing, on every filesystem.
    pub fn touch(&self) {
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let _ = fs::write(used_marker(&self.dir), now.to_string());
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

fn used_marker(dir: &Path) -> PathBuf {
    dir.join(".used")
}

/// When this source was last opened.
///
/// Falls back to the directory's own mtime when the marker is missing or
/// unreadable — caches written by an older build have no marker, and treating
/// those as brand new would leak them forever while treating them as ancient
/// would delete a proxy someone is about to use. `atime` is deliberately not
/// consulted: `relatime` and `noatime` mounts make it unreliable to the point
/// of being misleading.
fn last_used(dir: &Path) -> Option<SystemTime> {
    if let Ok(text) = fs::read_to_string(used_marker(dir)) {
        if let Ok(secs) = text.trim().parse::<u64>() {
            return Some(SystemTime::UNIX_EPOCH + Duration::from_secs(secs));
        }
    }
    fs::metadata(dir).and_then(|m| m.modified()).ok()
}

/// Remove stale artifacts. Called at startup and again on exit.
///
/// Safe to run twice in quick succession — removal is idempotent, and the
/// second pass simply sees fewer entries. Safe with two copies of the app open,
/// with one caveat worth knowing: an entry is only kept alive by [`touch`] at
/// import, so a second copy that has held a source open for longer than
/// `MAX_AGE` can have its cache removed from under it. The files it already
/// opened survive on unix, and on Windows the removal fails and is ignored;
/// either way the worst case is a rebuilt proxy, not a wrong result.
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
        // A marker from the future (clock skew, a restored backup) yields no
        // age rather than a huge one, so the entry is kept rather than deleted.
        let age = last_used(&path)
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

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("clip-editor-cache-{name}"));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The original bug: `touch` had an effect on first import and never again,
    /// so a source opened every day was deleted on day 14 anyway. A second
    /// touch must move the recorded time forward.
    #[test]
    fn touching_twice_moves_the_recorded_time() {
        let dir = temp_dir("touch-twice");
        let cache = SourceCache { dir: dir.clone() };

        cache.touch();
        // Backdate well past MAX_AGE, as if this entry had been sitting here
        // since the first import, then open the source again.
        fs::write(used_marker(&dir), "1000").unwrap();
        let stale = last_used(&dir).unwrap();
        assert!(
            SystemTime::now().duration_since(stale).unwrap() > MAX_AGE,
            "backdated marker should read as expired"
        );

        cache.touch();
        let fresh = last_used(&dir).unwrap();
        assert!(fresh > stale, "a second touch must move the time forward");
        assert!(
            SystemTime::now().duration_since(fresh).unwrap() < MAX_AGE,
            "a just-touched entry must not read as expired"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    /// Caches written before the marker existed must not be treated as brand
    /// new (they would leak forever) or as ancient (a live proxy would go).
    #[test]
    fn entries_without_a_marker_fall_back_to_the_directory() {
        let dir = temp_dir("no-marker");
        let from_dir = last_used(&dir).expect("directory mtime is a usable fallback");
        assert!(SystemTime::now().duration_since(from_dir).unwrap() < MAX_AGE);

        // An unparseable marker is the same situation as no marker at all.
        fs::write(used_marker(&dir), "not a timestamp").unwrap();
        assert!(last_used(&dir).is_some());

        let _ = fs::remove_dir_all(&dir);
    }

    /// A run killed partway through the strip leaves files behind. Those are
    /// not a finished strip, and the next open has to be able to tell.
    #[test]
    fn a_half_written_thumbnail_strip_does_not_read_as_finished() {
        let dir = temp_dir("thumbs");
        let cache = SourceCache { dir: dir.clone() };
        cache.begin_thumbs();
        assert!(!cache.thumbs_complete());

        // ffmpeg got three of the frames out before it was killed.
        for i in 1..=3 {
            fs::write(dir.join("thumbs").join(format!("thumb_{i:05}.jpg")), b"x").unwrap();
        }
        assert_eq!(cache.thumbs().len(), 3);
        assert!(
            !cache.thumbs_complete(),
            "files on disk are not the same claim as a finished strip"
        );

        // Starting again clears what was there rather than generating around it.
        cache.begin_thumbs();
        assert!(cache.thumbs().is_empty());
        for i in 1..=5 {
            fs::write(dir.join("thumbs").join(format!("thumb_{i:05}.jpg")), b"x").unwrap();
        }
        cache.finish_thumbs();
        assert!(cache.thumbs_complete());
        // The marker is not a thumbnail.
        assert_eq!(cache.thumbs().len(), 5);

        let _ = fs::remove_dir_all(&dir);
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
