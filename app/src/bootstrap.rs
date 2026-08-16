//! First-run ffmpeg bootstrap: pinned download, hash verification, extract.
//!
//! The download is pinned to a specific build. A silent ffmpeg upgrade
//! changing filter or CLI behaviour underneath the app is a genuinely
//! unpleasant bug class, so updating these constants is a deliberate change.

use anyhow::{anyhow, bail, Context, Result};
use sha2::{Digest, Sha256};
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use crate::ffmpeg::{tools_in_dir, Origin, Tools};

/// Human-readable version of the pinned build, shown on the first-run screen
/// and stored next to the binaries so a changed pin triggers a re-download.
pub const PINNED_VERSION: &str = match () {
    #[cfg(target_os = "macos")]
    () => "ffmpeg 9.x (osxexperts, arm64)",
    #[cfg(not(target_os = "macos"))]
    () => "ffmpeg n8.1.2 (BtbN autobuild-2026-08-12-13-15)",
};

/// Where the ffmpeg source for the shipped build lives — the GPL obligation is
/// to link to the exact tarball in the release notes, and this is that link.
pub const SOURCE_URL: &str = "https://github.com/FFmpeg/FFmpeg/tree/release/8.1";

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
/// screen then offers only the "use your own ffmpeg" paths. That is the case
/// on Intel macOS: the only static builds available there are a different
/// ffmpeg major from the Apple Silicon ones, and shipping two different
/// ffmpeg versions to one platform is the silent-behaviour-difference problem
/// this pinning exists to avoid.
pub fn downloads() -> &'static [Download] {
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    {
        &[
            Download {
                url: "https://www.osxexperts.net/ffmpeg9arm.zip",
                sha256: "d0c06c5c68ce48af3143b262f7a9118a7c9f67de1e237fcc24ffb14df9c67af9",
                size: 22_608_364,
            },
            Download {
                url: "https://www.osxexperts.net/ffprobe9arm.zip",
                sha256: "0c94fbdd8917022f28115eca512196cf4648732bc9e5db9ec8896c7e519d02aa",
                size: 22_530_006,
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

    let tmp = dir.join("download.tmp");
    for d in downloads() {
        let digest = fetch(d.url, &tmp, &should_continue, |chunk| {
            done += chunk;
            on_progress(DownloadProgress {
                done_bytes: done.min(total),
                total_bytes: total,
            });
        })?;
        // Verify before extracting: running an unexpected binary is worse than
        // failing, so a mismatch fails loudly.
        if !digest.eq_ignore_ascii_case(d.sha256) {
            let _ = fs::remove_file(&tmp);
            bail!(
                "checksum mismatch for {}\n  expected {}\n  got      {}",
                d.url,
                d.sha256,
                digest
            );
        }
        extract(&tmp, &dir).with_context(|| format!("extracting {}", d.url))?;
        let _ = fs::remove_file(&tmp);
    }

    make_executable(&dir)?;
    fs::write(stamp_path(&dir), PINNED_VERSION)?;

    let tools = tools_in_dir(&dir, Origin::Managed)
        .ok_or_else(|| anyhow!("archive did not contain ffmpeg and ffprobe"))?;
    tools
        .verify()
        .context("the downloaded ffmpeg would not run")?;
    Ok(tools)
}

fn fetch(
    url: &str,
    dest: &Path,
    should_continue: &impl Fn() -> bool,
    mut on_chunk: impl FnMut(u64),
) -> Result<String> {
    let mut response = ureq::get(url)
        .call()
        .with_context(|| format!("requesting {url}"))?;
    let mut reader = response.body_mut().as_reader();
    let mut file = File::create(dest).with_context(|| format!("writing {}", dest.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 128 * 1024];
    loop {
        if !should_continue() {
            drop(file);
            let _ = fs::remove_file(dest);
            bail!("cancelled");
        }
        let n = reader.read(&mut buf).context("reading download")?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        file.write_all(&buf[..n])?;
        on_chunk(n as u64);
    }
    file.flush()?;
    Ok(hex(&hasher.finalize()))
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

fn wanted(name: &str) -> bool {
    matches!(name, "ffmpeg" | "ffprobe" | "ffmpeg.exe" | "ffprobe.exe")
}

/// Pull just the binaries out, wherever the archive happens to put them —
/// osxexperts zips them at the root, BtbN nests them under `<name>/bin/`.
fn extract(archive: &Path, dest: &Path) -> Result<()> {
    let name = archive.to_string_lossy().to_string();
    let _ = name;
    let mut file = File::open(archive)?;
    let mut magic = [0u8; 6];
    file.read_exact(&mut magic).context("empty archive")?;
    file.rewind_start()?;

    if &magic[..2] == b"PK" {
        extract_zip(file, dest)
    } else if magic == [0xfd, b'7', b'z', b'X', b'Z', 0x00] {
        extract_tar_xz(file, dest, archive)
    } else {
        bail!("unrecognised archive format")
    }
}

trait Rewind {
    fn rewind_start(&mut self) -> io::Result<()>;
}
impl Rewind for File {
    fn rewind_start(&mut self) -> io::Result<()> {
        use std::io::Seek;
        self.seek(io::SeekFrom::Start(0)).map(|_| ())
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
    // archives are >100 MB compressed.
    let tar_path = archive.with_extension("tar");
    {
        let mut reader = io::BufReader::new(&mut file);
        let mut out = io::BufWriter::new(File::create(&tar_path)?);
        lzma_rs::xz_decompress(&mut reader, &mut out).map_err(|e| anyhow!("xz decode: {e:?}"))?;
        out.flush()?;
    }
    let result = (|| -> Result<()> {
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
    })();
    let _ = fs::remove_file(&tar_path);
    result
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
            assert!(d.size > 1_000_000);
        }
        // Every platform we ship a build for has a pinned ffmpeg; Intel macOS
        // is source-only and deliberately has none.
        if cfg!(not(all(target_os = "macos", not(target_arch = "aarch64")))) {
            assert!(has_pinned_build());
            assert!(total_download_bytes() > 0);
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

    #[test]
    fn hex_is_lowercase_and_padded() {
        assert_eq!(hex(&[0x0f, 0xa0, 0x00]), "0fa000");
    }
}
