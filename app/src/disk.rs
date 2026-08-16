//! How much room is left on the volume a path lives on.
//!
//! There is no std API for this, so it is one syscall per platform. It exists
//! for one question — "is there room for this export?" — and answers `None`
//! rather than guessing when it cannot tell, because a warning that fires on a
//! failed `statvfs` would fire on network shares and be ignored from then on.

use std::path::Path;

/// The nearest existing ancestor of `path`.
///
/// The output file does not exist yet, and neither might its directory. The
/// volume is what is being asked about, and that is a property of whatever
/// part of the path is already there.
fn existing_ancestor(path: &Path) -> Option<&Path> {
    let mut cur = Some(path);
    while let Some(p) = cur {
        if p.exists() {
            return Some(p);
        }
        cur = p.parent();
    }
    None
}

/// Bytes available to this user on the volume holding `path`, if it can be
/// determined.
///
/// Available, not free: the two differ by the reserve most filesystems keep for
/// root, and it is the first number that decides whether a write succeeds.
pub fn available_bytes(path: &Path) -> Option<u64> {
    let dir = existing_ancestor(path)?;
    platform::available_bytes(dir)
}

#[cfg(unix)]
mod platform {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;

    pub fn available_bytes(dir: &Path) -> Option<u64> {
        let c_path = CString::new(dir.as_os_str().as_bytes()).ok()?;
        // SAFETY: `stat` is a valid, correctly sized destination, and the path
        // is a NUL-terminated C string that outlives the call.
        let stat = unsafe {
            let mut stat: libc::statvfs = std::mem::zeroed();
            if libc::statvfs(c_path.as_ptr(), &mut stat) != 0 {
                return None;
            }
            stat
        };
        // `f_frsize` is the fragment size blocks are counted in; `f_bsize` is
        // the preferred I/O size and is not always the same number.
        let block = if stat.f_frsize > 0 {
            stat.f_frsize as u64
        } else {
            stat.f_bsize as u64
        };
        (stat.f_bavail as u64).checked_mul(block)
    }

    /// The device id is what "same filesystem" means on unix, and it is already
    /// in the metadata — no second syscall and no guessing from the path.
    pub fn same_volume(a: &Path, b: &Path) -> Option<bool> {
        use std::os::unix::fs::MetadataExt;
        let a = std::fs::metadata(a).ok()?;
        let b = std::fs::metadata(b).ok()?;
        Some(a.dev() == b.dev())
    }
}

#[cfg(windows)]
mod platform {
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;

    pub fn available_bytes(dir: &Path) -> Option<u64> {
        let wide: Vec<u16> = dir.as_os_str().encode_wide().chain([0]).collect();
        let mut available: u64 = 0;
        // SAFETY: `wide` is NUL-terminated and outlives the call; the two
        // out-parameters we do not want are passed as null, which the API
        // documents as permitted.
        let ok = unsafe {
            GetDiskFreeSpaceExW(
                wide.as_ptr(),
                &mut available,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        (ok != 0).then_some(available)
    }

    /// The path prefix — `C:\`, or `\\server\share` — is the volume for every
    /// path an editor deals with. Directory mount points can break this, but
    /// they are rare enough that the cost of being wrong (one warning too many
    /// or too few) does not justify opening a handle per path to be sure.
    pub fn same_volume(a: &Path, b: &Path) -> Option<bool> {
        fn prefix(p: &Path) -> Option<String> {
            match p.components().next()? {
                std::path::Component::Prefix(pre) => {
                    Some(pre.as_os_str().to_string_lossy().to_lowercase())
                }
                _ => None,
            }
        }
        Some(prefix(a)? == prefix(b)?)
    }
}

#[cfg(not(any(unix, windows)))]
mod platform {
    use std::path::Path;

    pub fn available_bytes(_dir: &Path) -> Option<u64> {
        None
    }

    pub fn same_volume(_a: &Path, _b: &Path) -> Option<bool> {
        None
    }
}

/// Whether two paths live on the same volume, if that can be determined.
///
/// It matters because Phase A's intermediates and the finished file exist at
/// the same moment: on one volume the requirement is their sum, on two it is
/// each against its own. `None` — unknown — is treated by callers as "two",
/// since inventing a warning is worse than missing one.
pub fn same_volume(a: &Path, b: &Path) -> Option<bool> {
    let (a, b) = (existing_ancestor(a)?, existing_ancestor(b)?);
    platform::same_volume(a, b)
}

/// `4.2 GB`, for a sentence rather than a table. Decimal units, because that is
/// what a disk's own "free space" says on every platform this runs on.
pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [(&str, u64); 4] = [
        ("TB", 1_000_000_000_000),
        ("GB", 1_000_000_000),
        ("MB", 1_000_000),
        ("kB", 1_000),
    ];
    for (unit, size) in UNITS {
        if bytes >= size {
            let value = bytes as f64 / size as f64;
            let digits = if value < 10.0 { 1 } else { 0 };
            return format!("{value:.digits$} {unit}");
        }
    }
    format!("{bytes} bytes")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_volume_that_exists_reports_a_plausible_number() {
        let dir = std::env::temp_dir();
        // Every platform this ships on supports the call; a `None` here would
        // silently disable the export space check.
        let available = available_bytes(&dir).expect("the temp volume has a size");
        assert!(available > 0);

        // The path being asked about does not have to exist — the output file
        // is the whole point, and it never does yet.
        let not_yet = dir.join("no-such-dir").join("export.mp4");
        assert_eq!(available_bytes(&not_yet), Some(available));
    }

    #[test]
    fn an_unrooted_path_is_unknown_rather_than_wrong() {
        assert_eq!(available_bytes(Path::new("")), None);
    }

    /// Scratch and output on one volume have to fit together, on two they do
    /// not — so this is the difference between clearing an export that cannot
    /// finish and inventing a warning about one that can.
    #[test]
    fn two_paths_on_the_temp_volume_are_one_volume() {
        let dir = std::env::temp_dir();
        assert_eq!(same_volume(&dir, &dir.join("a").join("b.mp4")), Some(true));
        // Unknown stays unknown; callers read `None` as "assume separate".
        assert_eq!(same_volume(Path::new(""), &dir), None);
    }

    #[test]
    fn sizes_read_the_way_a_disk_reports_them() {
        assert_eq!(human_bytes(0), "0 bytes");
        assert_eq!(human_bytes(999), "999 bytes");
        assert_eq!(human_bytes(1_500), "1.5 kB");
        assert_eq!(human_bytes(8_400_000_000), "8.4 GB");
        assert_eq!(human_bytes(42_000_000_000), "42 GB");
    }
}
