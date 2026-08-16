//! Every symbol the UI draws, in one place.
//!
//! egui's bundled fonts cover a specific subset — the obvious choice for an
//! icon is often not in it, and a missing glyph renders as a tofu box, which
//! reads as a broken app rather than a missing font. `tests/glyphs.rs` asserts
//! that everything here actually renders, so a new symbol cannot sneak in
//! untested.

pub const PLAY: &str = "⏵";
pub const PAUSE: &str = "⏸";
pub const STEP_BACK: &str = "⏴|";
pub const STEP_FORWARD: &str = "|⏵";
pub const GO_START: &str = "⏮";
pub const GO_END: &str = "⏭";
pub const UNDO: &str = "⟲";
pub const REDO: &str = "⟳";
pub const REMOVE: &str = "×";
pub const MIDDLE_DOT: &str = "·";

/// Checked by the glyph coverage test.
pub const ALL: &[&str] = &[
    PLAY,
    PAUSE,
    STEP_BACK,
    STEP_FORWARD,
    GO_START,
    GO_END,
    UNDO,
    REDO,
    REMOVE,
    MIDDLE_DOT,
];
