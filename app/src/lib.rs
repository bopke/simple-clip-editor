//! Clip Editor — a deliberately small desktop video clip editor.
//! Trim, crop, mix audio, export. Nothing else.

pub mod app;
pub mod audio;
pub mod bootstrap;
pub mod cache;
pub mod export;
pub mod ffmpeg;
pub mod glyphs;
pub mod import;
pub mod jobs;
pub mod keys;
pub mod playback;
pub mod ui_export;
pub mod ui_firstrun;
pub mod ui_preview;
pub mod ui_root;
pub mod ui_sidebar;
pub mod ui_timeline;
pub mod undo;
pub mod version;

pub use ui_root::run;
