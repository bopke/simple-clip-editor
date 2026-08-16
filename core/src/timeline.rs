//! Mapping between the output timeline and source positions.
//!
//! Preview and export share this. Playing across a cut must skip the removed
//! section, and it must skip it in exactly the place export will.

use crate::model::Segment;
use crate::time::{FrameIdx, Rational, RationalTime};

/// One kept range as source frame indices, `[start, end)`.
#[derive(Clone, Copy, Debug)]
struct Span {
    start: u64,
    end: u64,
}

impl Span {
    fn len(self) -> u64 {
        self.end.saturating_sub(self.start)
    }

    fn contains(self, f: u64) -> bool {
        f >= self.start && f < self.end
    }
}

/// The edit as whole frames of one rate.
///
/// Segments are times, but playback and export both walk a frame at a time, so
/// the rounding onto the grid happens once here rather than at every lookup.
#[derive(Clone, Debug, Default)]
pub struct Timeline {
    /// Kept ranges in output order, non-overlapping.
    spans: Vec<Span>,
    /// `starts[i]` is the output frame at which `spans[i]` begins.
    starts: Vec<u64>,
    total: u64,
}

impl Timeline {
    pub fn new(segments: &[Segment], fps: Rational) -> Timeline {
        let mut spans = Vec::with_capacity(segments.len());
        let mut starts = Vec::with_capacity(segments.len());
        let mut acc = 0u64;
        for s in segments {
            let (start, end) = s.frame_span(fps);
            let span = Span {
                start: start.0,
                end: end.0,
            };
            // A segment shorter than one frame of this grid rounds away to
            // nothing; keeping it would put a zero-length span in the middle
            // of the mapping for no frame to land on.
            if span.len() == 0 {
                continue;
            }
            starts.push(acc);
            acc += span.len();
            spans.push(span);
        }
        Timeline {
            spans,
            starts,
            total: acc,
        }
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
        Some(FrameIdx(self.spans[idx].start + off))
    }

    /// Source frame → output frame. `None` if the frame was cut out.
    pub fn to_output(&self, src: FrameIdx) -> Option<FrameIdx> {
        let idx = self.spans.iter().position(|s| s.contains(src.0))?;
        Some(FrameIdx(self.starts[idx] + (src.0 - self.spans[idx].start)))
    }

    /// Source frame → output frame, snapping a cut-out frame forward to the
    /// next kept frame. Used for playhead placement after an edit.
    pub fn to_output_snapped(&self, src: FrameIdx) -> Option<FrameIdx> {
        if let Some(o) = self.to_output(src) {
            return Some(o);
        }
        for (i, s) in self.spans.iter().enumerate() {
            if src.0 < s.start {
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
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fps() -> Rational {
        Rational::from_int(30)
    }

    fn tl() -> Timeline {
        Timeline::new(
            &[
                Segment::from_frames(FrameIdx(100), FrameIdx(200), fps()),
                Segment::from_frames(FrameIdx(500), FrameIdx(560), fps()),
            ],
            fps(),
        )
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
        let t = Timeline::new(&[], fps());
        assert!(t.is_empty());
        assert_eq!(t.to_source(FrameIdx(0)), None);
    }

    /// The same edit viewed at a coarser rate is the same wall-clock span,
    /// with the frame count that rate can represent.
    #[test]
    fn the_same_segments_map_at_another_rate() {
        let segs = [Segment::from_frames(FrameIdx(0), FrameIdx(300), fps())];
        let ntsc = Rational::new(24000, 1001);
        let t = Timeline::new(&segs, ntsc);
        // 10 s at 23.976 is 240 frames, against 300 at 30.
        assert_eq!(t.total_frames(), 240);
        assert_eq!(t.to_source(FrameIdx(0)), Some(FrameIdx(0)));
    }

    /// A segment too short to survive rounding onto the grid must not leave a
    /// zero-length hole that breaks the output→source mapping.
    #[test]
    fn a_sub_frame_segment_drops_out_of_the_mapping() {
        let ntsc = Rational::new(24000, 1001);
        let segs = [
            Segment::from_frames(FrameIdx(0), FrameIdx(60), fps()),
            // A hair over a millisecond: nothing at either rate.
            Segment {
                start: RationalTime::new(5, 1),
                end: RationalTime::new(5001, 1000),
            },
            Segment::from_frames(FrameIdx(300), FrameIdx(360), fps()),
        ];
        for rate in [fps(), ntsc] {
            let t = Timeline::new(&segs, rate);
            for o in 0..t.total_frames() {
                let s = t.to_source(FrameIdx(o)).expect("mapped");
                assert_eq!(t.to_output(s), Some(FrameIdx(o)), "at {rate:?}");
            }
        }
    }
}
