//! First-run ffmpeg bootstrap: pinned download, hash verification, extract.
//!
//! The download is pinned to a specific build. A silent ffmpeg upgrade
//! changing filter or CLI behaviour underneath the app is a genuinely
//! unpleasant bug class, so updating these constants is a deliberate change.
//!
//! A pin is only worth as much as the URL under it, so every URL here must be
//! **immutable** — a vendor that overwrites a file in place turns the checksum
//! from a guarantee into a scheduled outage, since the day they publish an
//! update is the day first-run stops working for everyone. That rules out
//! unversioned filenames however reputable the publisher: BtbN's dated
//! autobuild tags qualify, martin-riedl's timestamped release directories
//! qualify, `ffmpeg9arm.zip` does not.
//!
//! Each vendor also publishes its own SHA-256 next to the artifact, so the
//! constants below were cross-checked against the publisher's value rather
//! than merely recording whatever bytes happened to arrive here.

use anyhow::{anyhow, bail, Context, Result};
use sha2::{Digest, Sha256};
use std::fs::{self, File};
use std::io::{self, Read, Seek, Write};
use std::path::{Path, PathBuf};

use crate::ffmpeg::{tools_in_dir, Origin, Tools};

/// Human-readable version of the pinned build, shown on the first-run screen
/// and stored next to the binaries so a changed pin triggers a re-download.
pub const PINNED_VERSION: &str = match () {
    #[cfg(target_os = "macos")]
    () => "ffmpeg 9.0 (martin-riedl, arm64)",
    #[cfg(not(target_os = "macos"))]
    () => "ffmpeg n8.1.2 (BtbN autobuild-2026-08-12-13-15)",
};

/// Where the ffmpeg source for the shipped build lives — the GPL obligation is
/// to link to the exact source in the release notes, and this is that link.
///
/// One constant per platform because the vendors are not on the same major:
/// the macOS build is 9.0 and BtbN publishes no 9.x at all, so a single URL
/// cannot describe both without lying to one of them.
pub const SOURCE_URL: &str = match () {
    #[cfg(target_os = "macos")]
    () => "https://github.com/FFmpeg/FFmpeg/tree/release/9.0",
    #[cfg(not(target_os = "macos"))]
    () => "https://github.com/FFmpeg/FFmpeg/tree/release/8.1",
};

#[derive(Clone, Copy, Debug)]
pub struct Download {
    pub url: &'static str,
    pub sha256: &'static str,
    pub size: u64,
}

/// The pinned downloads for this platform. macOS ships ffmpeg and ffprobe as
/// separate archives; BtbN ships both in one.
///
/// Empty means there is no pinned build for this platform — the first-run
/// screen then offers only the "use your own ffmpeg" paths. Intel macOS is
/// empty because the release is Apple Silicon only (see
/// `scripts/bundle-macos.sh`); the vendor below does publish a matching 9.0
/// amd64 build at `/download/macos/amd64/`, so pinning it is a two-line change
/// if that ever changes.
pub fn downloads() -> &'static [Download] {
    // martin-riedl rather than osxexperts: the path is timestamped and
    // therefore immutable, the vendor publishes a `.sha256` beside each
    // archive, and the build is a tagged 9.0 release rather than a git
    // snapshot — so [`SOURCE_URL`] can name an exact release branch.
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    {
        &[
            Download {
                url:
                    "https://ffmpeg.martin-riedl.de/download/macos/arm64/1785863997_9.0/ffmpeg.zip",
                sha256: "5267ef149ee0d208057a1b316aac079b661b0476574dee5da7d225769773c603",
                size: 28_440_078,
            },
            Download {
                url:
                    "https://ffmpeg.martin-riedl.de/download/macos/arm64/1785863997_9.0/ffprobe.zip",
                sha256: "7778fbb533fb60d3336cbd9a9e51eced71658f020b570c7203590c1c41d42f50",
                size: 28_364_088,
            },
        ]
    }
    #[cfg(all(target_os = "macos", not(target_arch = "aarch64")))]
    {
        &[]
    }
    #[cfg(target_os = "windows")]
    {
        &[Download {
            url: "https://github.com/BtbN/FFmpeg-Builds/releases/download/autobuild-2026-08-12-13-15/ffmpeg-n8.1.2-34-g9b6c8969e0-win64-gpl-8.1.zip",
            sha256: "0bfebe51fced76c6cd5a42420fc2fc6e4646f95a556c59b0d7d298fcc24c6849",
            size: 167_988_754,
        }]
    }
    #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
    {
        &[Download {
            url: "https://github.com/BtbN/FFmpeg-Builds/releases/download/autobuild-2026-08-12-13-15/ffmpeg-n8.1.2-34-g9b6c8969e0-linuxarm64-gpl-8.1.tar.xz",
            sha256: "fbffa55263830f010355a44b2eb25dde32feb7b3870f1ec18737bfcd5e74df2b",
            size: 107_553_900,
        }]
    }
    #[cfg(all(target_os = "linux", not(target_arch = "aarch64")))]
    {
        &[Download {
            url: "https://github.com/BtbN/FFmpeg-Builds/releases/download/autobuild-2026-08-12-13-15/ffmpeg-n8.1.2-34-g9b6c8969e0-linux64-gpl-8.1.tar.xz",
            sha256: "980d92b6365c0bd242cbcc7a9c7a951acbae92a64285ca9db7c18999e62155a2",
            size: 125_512_244,
        }]
    }
}

pub fn total_download_bytes() -> u64 {
    downloads().iter().map(|d| d.size).sum()
}

/// Whether this platform has a build we can fetch at all.
pub fn has_pinned_build() -> bool {
    !downloads().is_empty()
}

/// Install location. The **data** dir, not the cache dir: cache is defined as
/// safe to delete at any time and on macOS it gets cleaned automatically.
/// Proxies belong in cache; the ffmpeg binary does not.
pub fn install_dir() -> Result<PathBuf> {
    let dirs = directories::ProjectDirs::from("", "", "clip-editor")
        .ok_or_else(|| anyhow!("no home directory"))?;
    Ok(dirs.data_dir().join("bin"))
}

fn stamp_path(dir: &Path) -> PathBuf {
    dir.join("version.txt")
}

/// Tools already installed and matching the pinned version.
///
/// The stamp is written last and only after the binary has proved it runs, so
/// a matching stamp means a complete install of the build we currently pin.
/// Changing [`PINNED_VERSION`] is therefore what triggers a re-download.
pub fn installed() -> Option<Tools> {
    let dir = install_dir().ok()?;
    let stamp = fs::read_to_string(stamp_path(&dir)).ok()?;
    if stamp.trim() != PINNED_VERSION {
        return None;
    }
    tools_in_dir(&dir, Origin::Managed)
}

/// Resolution order used at startup: explicit override, then our own install,
/// then whatever the user previously chose, then nothing (first-run screen).
pub fn locate(remembered_system_dir: Option<&Path>) -> Option<Tools> {
    if let Some(t) = crate::ffmpeg::from_env() {
        return Some(t);
    }
    if let Some(t) = installed() {
        return Some(t);
    }
    if let Some(dir) = remembered_system_dir {
        if let Some(t) = tools_in_dir(dir, Origin::System) {
            return Some(t);
        }
    }
    None
}

#[derive(Clone, Copy, Debug)]
pub struct DownloadProgress {
    pub done_bytes: u64,
    pub total_bytes: u64,
}

/// Download, verify, extract. `on_progress` is called as bytes arrive;
/// returning `false` from `should_continue` aborts.
pub fn install(
    mut on_progress: impl FnMut(DownloadProgress),
    should_continue: impl Fn() -> bool,
) -> Result<Tools> {
    if !has_pinned_build() {
        bail!("no pinned ffmpeg build for this platform — point the app at your own");
    }
    let dir = install_dir()?;
    fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let total = total_download_bytes();
    let mut done = 0u64;

    for d in downloads() {
        // Named after the URL, not a fixed `download.tmp`: macOS fetches two
        // archives and a second copy of the app may be doing the same thing at
        // the same time, and two writers on one path corrupt both downloads.
        let tmp = TempFile::new(dir.join(format!("{}.part", temp_stem(d.url))));
        let digest = fetch(d.url, &tmp.path, d.size, &should_continue, |chunk| {
            done += chunk;
            on_progress(DownloadProgress {
                done_bytes: done.min(total),
                total_bytes: total,
            });
        })?;
        // Verify before extracting: running an unexpected binary is worse than
        // failing, so a mismatch fails loudly.
        if !digest.eq_ignore_ascii_case(d.sha256) {
            bail!(
                "checksum mismatch for {}\n  expected {}\n  got      {}",
                d.url,
                d.sha256,
                digest
            );
        }
        extract(&tmp.path, &dir).with_context(|| format!("extracting {}", d.url))?;
    }

    make_executable(&dir)?;

    let tools = tools_in_dir(&dir, Origin::Managed)
        .ok_or_else(|| anyhow!("archive did not contain ffmpeg and ffprobe"))?;
    tools
        .verify()
        .context("the downloaded ffmpeg would not run")?;
    // Written last, and only once the binary has proved it runs, so a stamp
    // never advertises a half-finished install.
    fs::write(stamp_path(&dir), PINNED_VERSION)?;
    Ok(tools)
}

/// A scratch file that is removed however its scope is left.
///
/// A `?` on the extract path used to skip the `remove_file` on the line below
/// it and leave 168 MB in the data directory, where nothing ever collects it.
struct TempFile {
    path: PathBuf,
}

impl TempFile {
    fn new(path: PathBuf) -> TempFile {
        TempFile { path }
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// A filename derived from the URL, so two archives fetched in one run — and
/// two copies of the app fetching at once — do not land on the same path.
fn temp_stem(url: &str) -> String {
    let tail = url.rsplit('/').next().unwrap_or("download");
    let stem: String = tail
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
        .collect();
    // `.` and `..` survive the filter and are directories, not names.
    if stem.is_empty() || stem.chars().all(|c| c == '.') {
        "download".to_string()
    } else {
        stem
    }
}

/// How much larger than the pinned size a response may be before it is refused.
///
/// The size is pinned alongside the checksum, so anything materially bigger is
/// either the wrong artifact or a server that will happily stream until the
/// disk is full. Small slack absorbs a vendor re-packing the same build.
const SIZE_SLACK: u64 = 4 * 1024 * 1024;

/// Timeouts for the download.
///
/// A stalled connection with no deadline hangs the bootstrap thread, and the
/// only way out is a cancel button the user has to know to press. `ureq`'s
/// timeouts are deadlines rather than idle timers, so the body one has to be
/// generous — it is a >100 MB file and someone is on hotel wifi — while the
/// handshake ones can be short, because that is where a dead host shows up.
fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_resolve(Some(std::time::Duration::from_secs(30)))
        .timeout_connect(Some(std::time::Duration::from_secs(30)))
        .timeout_send_request(Some(std::time::Duration::from_secs(30)))
        .timeout_recv_response(Some(std::time::Duration::from_secs(60)))
        .timeout_recv_body(Some(std::time::Duration::from_secs(30 * 60)))
        .build()
        .into()
}

fn fetch(
    url: &str,
    dest: &Path,
    expected_size: u64,
    should_continue: &impl Fn() -> bool,
    mut on_chunk: impl FnMut(u64),
) -> Result<String> {
    let mut response = agent()
        .get(url)
        .call()
        .with_context(|| format!("requesting {url}"))?;
    let mut reader = response.body_mut().as_reader();
    let mut file = File::create(dest).with_context(|| format!("writing {}", dest.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 128 * 1024];
    let ceiling = expected_size.saturating_add(SIZE_SLACK);
    let mut written = 0u64;
    loop {
        if !should_continue() {
            bail!("cancelled");
        }
        let n = reader.read(&mut buf).context("reading download")?;
        if n == 0 {
            break;
        }
        written += n as u64;
        if written > ceiling {
            bail!(
                "{url} is larger than the pinned {expected_size} bytes — refusing to \
                 keep downloading"
            );
        }
        hasher.update(&buf[..n]);
        file.write_all(&buf[..n])?;
        on_chunk(n as u64);
    }
    file.flush()?;
    Ok(crate::hex::encode(&hasher.finalize()))
}

fn wanted(name: &str) -> bool {
    matches!(name, "ffmpeg" | "ffprobe" | "ffmpeg.exe" | "ffprobe.exe")
}

/// Pull just the binaries out, wherever the archive happens to put them —
/// martin-riedl zips them at the root, BtbN nests them under `<name>/bin/`.
fn extract(archive: &Path, dest: &Path) -> Result<()> {
    let mut file = File::open(archive).with_context(|| format!("opening {}", archive.display()))?;
    let mut magic = [0u8; 6];
    // Naming the file matters here: the caller's context says which *URL* was
    // being extracted, which is not the same as which file on disk is short.
    file.read_exact(&mut magic)
        .with_context(|| format!("{} is empty or truncated", archive.display()))?;
    file.rewind()?;

    if &magic[..2] == b"PK" {
        extract_zip(file, dest)
    } else if magic == [0xfd, b'7', b'z', b'X', b'Z', 0x00] {
        extract_tar_xz(file, dest, archive)
    } else {
        bail!(
            "unrecognised archive format in {} (magic {magic:02x?})",
            archive.display()
        )
    }
}

fn extract_zip(file: File, dest: &Path) -> Result<()> {
    let mut zip = zip::ZipArchive::new(file)?;
    for i in 0..zip.len() {
        let mut entry = zip.by_index(i)?;
        if !entry.is_file() {
            continue;
        }
        let Some(path) = entry.enclosed_name() else {
            continue;
        };
        // macOS zips carry a __MACOSX shadow tree of resource forks.
        if path.components().any(|c| c.as_os_str() == "__MACOSX") {
            continue;
        }
        let Some(name) = path.file_name().map(|n| n.to_string_lossy().to_string()) else {
            continue;
        };
        if !wanted(&name) {
            continue;
        }
        let out = dest.join(&name);
        let mut f = File::create(&out).with_context(|| format!("writing {}", out.display()))?;
        io::copy(&mut entry, &mut f)?;
    }
    Ok(())
}

fn extract_tar_xz(mut file: File, dest: &Path, archive: &Path) -> Result<()> {
    // Decompress to a sibling temp file rather than into memory; these
    // archives are >100 MB compressed. The guard matters as much here as on the
    // download: a failed xz decode leaves the largest file the app ever writes.
    let tar = TempFile::new(archive.with_extension("tar"));
    let tar_path = tar.path.clone();
    {
        let mut reader = io::BufReader::new(&mut file);
        let mut out = io::BufWriter::new(File::create(&tar_path)?);
        lzma_rs::xz_decompress(&mut reader, &mut out).map_err(|e| anyhow!("xz decode: {e:?}"))?;
        out.flush()?;
    }
    (|| -> Result<()> {
        let mut tar = tar::Archive::new(File::open(&tar_path)?);
        for entry in tar.entries()? {
            let mut entry = entry?;
            let path = entry.path()?.into_owned();
            let Some(name) = path.file_name().map(|n| n.to_string_lossy().to_string()) else {
                continue;
            };
            if !wanted(&name) {
                continue;
            }
            let out = dest.join(&name);
            let mut f = File::create(&out)?;
            io::copy(&mut entry, &mut f)?;
        }
        Ok(())
    })()
}

#[cfg(unix)]
fn make_executable(dir: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    for name in ["ffmpeg", "ffprobe"] {
        let path = dir.join(name);
        if path.exists() {
            // Files written by our own process are not quarantined on macOS,
            // so this is enough to make them runnable — no codesign dance.
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755))?;
        }
    }
    Ok(())
}

#[cfg(not(unix))]
fn make_executable(_dir: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pinned_downloads_look_sane() {
        for d in downloads() {
            assert!(d.url.starts_with("https://"), "{}", d.url);
            assert_eq!(d.sha256.len(), 64, "{}", d.url);
            assert!(d.sha256.chars().all(|c| c.is_ascii_hexdigit()), "{}", d.url);
            assert!(d.size > 1_000_000);
        }
        // Every platform we ship a build for has a pinned ffmpeg; Intel macOS
        // is source-only and deliberately has none.
        if cfg!(not(all(target_os = "macos", not(target_arch = "aarch64")))) {
            assert!(has_pinned_build());
            assert!(total_download_bytes() > 0);
        }
    }

    /// A checksum pin is only as good as the URL beneath it: if the vendor can
    /// overwrite that path, the pin stops being a guarantee and becomes a
    /// scheduled outage on the day they publish an update. Every URL must
    /// therefore carry something that makes it immutable — a dated tag, a
    /// timestamped directory, a version in the filename.
    #[test]
    fn pinned_urls_are_immutable() {
        for d in downloads() {
            let immutable = d.url.contains("autobuild-") || d.url.contains("_9.0/");
            assert!(
                immutable,
                "{} has no version or timestamp in its path, so the vendor can \
                 replace it and break every first run",
                d.url
            );
        }
    }

    /// The source link is a licence obligation, so it has to name the release
    /// the pinned build was actually cut from. The vendors are on different
    /// majors; this is what stops the two drifting apart again.
    #[test]
    fn the_source_url_matches_the_pinned_build() {
        let (branch, marker) = if cfg!(target_os = "macos") {
            ("release/9.0", "_9.0/")
        } else {
            ("release/8.1", "n8.1.2")
        };
        assert!(
            SOURCE_URL.ends_with(branch),
            "{SOURCE_URL} does not point at {branch}"
        );
        for d in downloads() {
            assert!(
                d.url.contains(marker),
                "{} is not the {branch} build that SOURCE_URL advertises",
                d.url
            );
        }
    }

    #[test]
    fn install_refuses_when_there_is_nothing_pinned() {
        if !has_pinned_build() {
            let err = install(|_| {}, || true).unwrap_err();
            assert!(err.to_string().contains("no pinned ffmpeg build"));
        }
    }

    #[test]
    fn only_the_binaries_are_extracted() {
        assert!(wanted("ffmpeg"));
        assert!(wanted("ffprobe.exe"));
        assert!(!wanted("ffplay"));
        assert!(!wanted("README.txt"));
    }

    /// macOS fetches two archives in one run, and a second copy of the app may
    /// be fetching at the same moment. One fixed `download.tmp` for all of them
    /// is two writers on one path.
    #[test]
    fn each_download_gets_its_own_temp_name() {
        let names: Vec<String> = downloads().iter().map(|d| temp_stem(d.url)).collect();
        for (i, a) in names.iter().enumerate() {
            for b in &names[i + 1..] {
                assert_ne!(a, b, "two downloads share a temp file");
            }
        }
        assert_eq!(temp_stem("https://host/dir/ffmpeg.zip"), "ffmpeg.zip");
        // Only the last component, and only characters that are a filename —
        // nothing from a URL becomes a path traversal or a shell surprise.
        assert_eq!(temp_stem("https://host/../../etc/pa$$wd"), "pawd");
        assert_eq!(temp_stem("https://host/dir/.."), "download");
        assert_eq!(temp_stem("https://host/"), "download");
    }

    /// The temp file has to go whichever way the scope is left — the original
    /// bug was a `?` on extraction skipping the cleanup on the next line.
    #[test]
    fn the_temp_file_is_removed_even_when_extraction_fails() {
        let dir = std::env::temp_dir().join("clip-editor-tempfile-test");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("big.part");

        let err = (|| -> Result<()> {
            let tmp = TempFile::new(path.clone());
            fs::write(&tmp.path, b"not an archive")?;
            assert!(tmp.path.exists());
            extract(&tmp.path, &dir)?;
            Ok(())
        })();
        assert!(err.is_err());
        assert!(!path.exists(), "a failed run left its scratch file behind");

        let _ = fs::remove_dir_all(&dir);
    }

    /// A pinned size is a ceiling as well as a progress denominator; without
    /// one, a misbehaving server streams until the disk is full.
    #[test]
    fn the_size_ceiling_is_close_to_the_pinned_size() {
        for d in downloads() {
            let ceiling = d.size + SIZE_SLACK;
            assert!(
                ceiling < d.size * 2,
                "{} would accept twice what it pinned",
                d.url
            );
        }
    }

    /// A failed extraction has to say *which file*: the caller only knows the
    /// URL it was fetching, and on macOS two archives land on the same
    /// `download.tmp`, so "unrecognised archive format" alone identifies
    /// nothing.
    #[test]
    fn extraction_errors_name_the_archive() {
        let dir = std::env::temp_dir().join("clip-editor-extract-test");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let junk = dir.join("junk.bin");
        fs::write(&junk, b"not an archive").unwrap();
        let err = extract(&junk, &dir).unwrap_err().to_string();
        assert!(err.contains("unrecognised archive format"), "{err}");
        assert!(err.contains("junk.bin"), "{err}");

        let empty = dir.join("empty.bin");
        fs::write(&empty, b"").unwrap();
        let err = extract(&empty, &dir).unwrap_err().to_string();
        assert!(err.contains("empty.bin"), "{err}");

        let _ = fs::remove_dir_all(&dir);
    }
}
