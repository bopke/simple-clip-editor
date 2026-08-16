//! Stamps the build with the commit it came from, so a user reporting a
//! problem can say which build they are running.

use std::process::Command;

fn main() {
    let hash = git_short_hash()
        // CI checkouts sometimes have no usable git history, but they do
        // export the commit.
        .or_else(|| {
            std::env::var("GITHUB_SHA")
                .ok()
                .map(|s| s[..7.min(s.len())].to_string())
        })
        .unwrap_or_else(|| "dev".to_string());
    println!("cargo:rustc-env=CLIP_GIT_HASH={hash}");

    let dirty = git_is_dirty();
    println!(
        "cargo:rustc-env=CLIP_GIT_DIRTY={}",
        if dirty { "+" } else { "" }
    );

    // Rebuild when the checked-out commit changes.
    for path in [".git/HEAD", "../.git/HEAD", "../.git/index"] {
        if std::path::Path::new(path).exists() {
            println!("cargo:rerun-if-changed={path}");
        }
    }
    println!("cargo:rerun-if-env-changed=GITHUB_SHA");
}

fn git_short_hash() -> Option<String> {
    let out = Command::new("git")
        .args(["rev-parse", "--short=7", "HEAD"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let hash = String::from_utf8(out.stdout).ok()?.trim().to_string();
    (!hash.is_empty()).then_some(hash)
}

fn git_is_dirty() -> bool {
    Command::new("git")
        .args(["status", "--porcelain", "--untracked-files=no"])
        .output()
        .map(|o| o.status.success() && !o.stdout.is_empty())
        .unwrap_or(false)
}
