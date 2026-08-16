//! Pure edit-model and command-construction logic. No UI dependencies, no
//! process spawning — everything here is unit-testable without rendering or
//! running anything.

pub mod audio_math;
pub mod command;
pub mod crop;
pub mod edits;
pub mod model;
pub mod probe;
pub mod progress;
pub mod time;
pub mod timeline;

pub use model::{
    AspectMode, AudioSourceSelection, AudioStreamInfo, AudioTrack, CropRect, OriginalAudio,
    OutputPreset, OutputSettings, Project, Quality, QualityBucket, Segment, SourceInfo, TrackId,
};
pub use time::{FrameIdx, Rational, RationalTime};
pub use timeline::Timeline;
