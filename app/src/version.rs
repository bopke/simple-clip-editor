//! Build identity. The commit is the version — there is no release train to
//! track, so "which commit is this" is the only question worth answering.

/// Short commit hash this binary was built from, `dev` outside a checkout.
/// A trailing `+` means the working tree had uncommitted changes.
pub const COMMIT: &str = concat!(env!("CLIP_GIT_HASH"), env!("CLIP_GIT_DIRTY"));

/// Window title: `Clip Editor version 31adf42`.
pub fn window_title() -> String {
    format!("Clip Editor version {COMMIT}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn title_carries_the_commit() {
        let title = window_title();
        assert!(title.starts_with("Clip Editor version "), "{title}");
        assert!(!COMMIT.is_empty());
    }
}
