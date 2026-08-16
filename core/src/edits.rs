//! Segment editing operations. A single contiguous trim is just
//! `segments.len() == 1`, so none of this special-cases the one-range case.
//!
//! Everything here works in [`RationalTime`], because that is what a segment
//! boundary is. Callers hold a playhead in frames and convert on the way in —
//! the frame the user is looking at is exactly a time, and which grid it came
//! from stops mattering the moment it has been converted.

use crate::model::Segment;
use crate::time::RationalTime;

/// Index of the segment containing `t`, if any.
pub fn segment_at(segments: &[Segment], t: RationalTime) -> Option<usize> {
    segments.iter().position(|s| s.contains(t))
}

/// The segment containing `t`, or the next one after it.
pub fn segment_at_or_after(segments: &[Segment], t: RationalTime) -> Option<usize> {
    segment_at(segments, t).or_else(|| segments.iter().position(|s| s.start > t))
}

/// Set the in-point of the segment under the playhead.
pub fn set_in(segments: &mut Vec<Segment>, at: RationalTime) {
    let Some(i) = segment_at_or_after(segments, at) else {
        return;
    };
    if at < segments[i].end {
        segments[i].start = at;
    }
    prune(segments);
}

/// Set the out-point of the segment under the playhead. `end` is exclusive:
/// the caller passes the start of the frame *after* the one being kept.
pub fn set_out(segments: &mut Vec<Segment>, end: RationalTime) {
    // `end` lands one frame past the kept frame, so on the last frame of a
    // segment it is the segment's own end and `contains` misses it.
    let Some(i) = segment_at(segments, end).or_else(|| segments.iter().rposition(|s| s.end <= end))
    else {
        return;
    };
    if end > segments[i].start {
        segments[i].end = end;
    }
    prune(segments);
}

/// Remove `[from, to)` from the kept ranges, splitting a segment in two if the
/// range falls inside one. This is the cut.
pub fn delete_range(segments: &mut Vec<Segment>, from: RationalTime, to: RationalTime) {
    if to <= from {
        return;
    }
    let mut out: Vec<Segment> = Vec::with_capacity(segments.len() + 1);
    for s in segments.iter().copied() {
        // No overlap.
        if s.end <= from || s.start >= to {
            out.push(s);
            continue;
        }
        if s.start < from {
            out.push(Segment {
                start: s.start,
                end: from,
            });
        }
        if s.end > to {
            out.push(Segment {
                start: to,
                end: s.end,
            });
        }
    }
    *segments = out;
    prune(segments);
}

/// Keep only `[from, to)`.
pub fn keep_only(segments: &mut Vec<Segment>, from: RationalTime, to: RationalTime) {
    let mut out: Vec<Segment> = Vec::new();
    for s in segments.iter().copied() {
        let seg = Segment {
            start: s.start.max(from),
            end: s.end.min(to),
        };
        if !seg.is_empty() {
            out.push(seg);
        }
    }
    *segments = out;
    prune(segments);
}

/// Split the segment under the playhead in two. Both halves are kept; this is
/// how a cut point gets created before deleting one side.
pub fn split_at(segments: &mut Vec<Segment>, at: RationalTime) {
    let Some(i) = segment_at(segments, at) else {
        return;
    };
    let s = segments[i];
    if at <= s.start || at >= s.end {
        return;
    }
    segments[i] = Segment {
        start: s.start,
        end: at,
    };
    segments.insert(
        i + 1,
        Segment {
            start: at,
            end: s.end,
        },
    );
}

/// Drop empty segments, keeping order.
fn prune(segments: &mut Vec<Segment>) {
    segments.retain(|s| !s.is_empty());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::time::{FrameIdx, Rational};

    /// Every test here works at 30 fps and names its edit points in frames,
    /// which is how the UI produces them.
    fn fps() -> Rational {
        Rational::from_int(30)
    }

    fn t(frame: u64) -> RationalTime {
        FrameIdx(frame).to_time(fps())
    }

    fn segs(v: &[(u64, u64)]) -> Vec<Segment> {
        v.iter()
            .map(|(a, b)| Segment::from_frames(FrameIdx(*a), FrameIdx(*b), fps()))
            .collect()
    }

    #[test]
    fn in_and_out_trim_the_segment_under_the_playhead() {
        let mut s = segs(&[(0, 1000)]);
        set_in(&mut s, t(100));
        assert_eq!(s, segs(&[(100, 1000)]));
        set_out(&mut s, t(500));
        assert_eq!(s, segs(&[(100, 500)]));
    }

    #[test]
    fn delete_inside_a_segment_splits_it() {
        let mut s = segs(&[(0, 1000)]);
        delete_range(&mut s, t(200), t(300));
        assert_eq!(s, segs(&[(0, 200), (300, 1000)]));
    }

    #[test]
    fn delete_spanning_segments_trims_both_ends() {
        let mut s = segs(&[(0, 200), (300, 1000)]);
        delete_range(&mut s, t(100), t(400));
        assert_eq!(s, segs(&[(0, 100), (400, 1000)]));
    }

    #[test]
    fn delete_covering_a_whole_segment_removes_it() {
        let mut s = segs(&[(0, 200), (300, 400), (500, 600)]);
        delete_range(&mut s, t(250), t(450));
        assert_eq!(s, segs(&[(0, 200), (500, 600)]));
    }

    #[test]
    fn delete_outside_everything_is_a_no_op() {
        let mut s = segs(&[(0, 200)]);
        delete_range(&mut s, t(500), t(600));
        delete_range(&mut s, t(50), t(50));
        assert_eq!(s, segs(&[(0, 200)]));
    }

    #[test]
    fn split_creates_two_adjacent_segments() {
        let mut s = segs(&[(0, 1000)]);
        split_at(&mut s, t(400));
        assert_eq!(s, segs(&[(0, 400), (400, 1000)]));
        // Splitting on a boundary changes nothing.
        split_at(&mut s, t(400));
        assert_eq!(s, segs(&[(0, 400), (400, 1000)]));
    }

    #[test]
    fn keep_only_clips_every_segment() {
        let mut s = segs(&[(0, 200), (300, 1000)]);
        keep_only(&mut s, t(150), t(400));
        assert_eq!(s, segs(&[(150, 200), (300, 400)]));
    }

    #[test]
    fn in_point_past_the_end_is_ignored() {
        let mut s = segs(&[(0, 100)]);
        set_in(&mut s, t(500));
        assert_eq!(s, segs(&[(0, 100)]));
    }

    /// An edit made while viewing 23.976 lands on that grid exactly, not on
    /// the rate the segment happened to be created at.
    fn ntsc() -> Rational {
        Rational::new(24000, 1001)
    }

    #[test]
    fn an_edit_at_another_rate_lands_on_that_rate() {
        let mut s = segs(&[(0, 1000)]);
        let at = FrameIdx(100).to_time(ntsc());
        split_at(&mut s, at);
        assert_eq!(s[0].end, at);
        // And it reads back as frame 100 of the grid it was made on.
        assert_eq!(s[0].end_frame(ntsc()), FrameIdx(100));
    }
}
