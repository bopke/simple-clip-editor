//! Mapping between the output timeline and source positions.
//!
//! Preview and export share this. Playing across a cut must skip the removed
//! section, and it must skip it in exactly the place export will.

use crate::model::Segment;
use crate::time::{FrameIdx, Rational, RationalTime};

#[derive(Clone, Debug, Default)]
pub struct Timeline {
    /// Kept ranges in output order, non-overlapping.
    segments: Vec<Segment>,
    /// `starts[i]` is the output frame at which `segments[i]` begins.
    starts: Vec<u64>,
    total: u64,
}

impl Timeline {
    pub fn new(segments: Vec<Segment>) -> Timeline {
        let mut starts = Vec::with_capacity(segments.len());
        let mut acc = 0u64;
        for s in &segments {
            starts.push(acc);
            acc += s.len();
        }
        Timeline {
            segments,
            starts,
            total: acc,
        }
    }

    pub fn segments(&self) -> &[Segment] {
        &self.segments
    }

    pub fn total_frames(&self) -> u64 {
        self.total
    }

    pub fn is_empty(&self) -> bool {
        self.total == 0
    }

    pub fn duration(&self, fps: Rational) -> RationalTime {
        FrameIdx(self.total).to_time(fps)
    }

    /// Output frame → source frame. `None` past the end.
    pub fn to_source(&self, out: FrameIdx) -> Option<FrameIdx> {
        let idx = self.segment_at_output(out)?;
        let off = out.0 - self.starts[idx];
        Some(FrameIdx(self.segments[idx].start.0 + off))
    }

    /// Source frame → output frame. `None` if the frame was cut out.
    pub fn to_output(&self, src: FrameIdx) -> Option<FrameIdx> {
        let idx = self.segments.iter().position(|s| s.contains(src))?;
        Some(FrameIdx(
            self.starts[idx] + (src.0 - self.segments[idx].start.0),
        ))
    }

    /// Source frame → output frame, snapping a cut-out frame forward to the
    /// next kept frame. Used for playhead placement after an edit.
    pub fn to_output_snapped(&self, src: FrameIdx) -> Option<FrameIdx> {
        if let Some(o) = self.to_output(src) {
            return Some(o);
        }
        for (i, s) in self.segments.iter().enumerate() {
            if src.0 < s.start.0 {
                return Some(FrameIdx(self.starts[i]));
            }
        }
        None
    }

    /// Index of the segment containing an output frame.
    pub fn segment_at_output(&self, out: FrameIdx) -> Option<usize> {
        if out.0 >= self.total {
            return None;
        }
        // starts is sorted; partition_point gives the first start > out.
        let idx = self.starts.partition_point(|&s| s <= out.0);
        idx.checked_sub(1)
    }

    /// Output frame where each segment begins — the cut points, for drawing.
    pub fn cut_points(&self) -> &[u64] {
        &self.starts
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tl() -> Timeline {
        Timeline::new(vec![
            Segment {
                start: FrameIdx(100),
                end: FrameIdx(200),
            },
            Segment {
                start: FrameIdx(500),
                end: FrameIdx(560),
            },
        ])
    }

    #[test]
    fn maps_output_to_source_across_a_cut() {
        let t = tl();
        assert_eq!(t.total_frames(), 160);
        assert_eq!(t.to_source(FrameIdx(0)), Some(FrameIdx(100)));
        assert_eq!(t.to_source(FrameIdx(99)), Some(FrameIdx(199)));
        // The frame right after the cut jumps to the second segment.
        assert_eq!(t.to_source(FrameIdx(100)), Some(FrameIdx(500)));
        assert_eq!(t.to_source(FrameIdx(159)), Some(FrameIdx(559)));
        assert_eq!(t.to_source(FrameIdx(160)), None);
    }

    #[test]
    fn maps_source_to_output() {
        let t = tl();
        assert_eq!(t.to_output(FrameIdx(100)), Some(FrameIdx(0)));
        assert_eq!(t.to_output(FrameIdx(500)), Some(FrameIdx(100)));
        assert_eq!(t.to_output(FrameIdx(300)), None);
        assert_eq!(t.to_output_snapped(FrameIdx(300)), Some(FrameIdx(100)));
        assert_eq!(t.to_output_snapped(FrameIdx(9999)), None);
    }

    #[test]
    fn roundtrips_every_kept_frame() {
        let t = tl();
        for o in 0..t.total_frames() {
            let s = t.to_source(FrameIdx(o)).unwrap();
            assert_eq!(t.to_output(s), Some(FrameIdx(o)));
        }
    }

    #[test]
    fn empty_timeline_is_harmless() {
        let t = Timeline::new(vec![]);
        assert!(t.is_empty());
        assert_eq!(t.to_source(FrameIdx(0)), None);
    }
}
