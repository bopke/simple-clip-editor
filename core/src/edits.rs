//! Segment editing operations. A single contiguous trim is just
//! `segments.len() == 1`, so none of this special-cases the one-range case.

use crate::model::Segment;
use crate::time::FrameIdx;

/// Index of the segment containing `f`, if any.
pub fn segment_at(segments: &[Segment], f: FrameIdx) -> Option<usize> {
    segments.iter().position(|s| s.contains(f))
}

/// The segment containing `f`, or the next one after it.
pub fn segment_at_or_after(segments: &[Segment], f: FrameIdx) -> Option<usize> {
    segment_at(segments, f).or_else(|| segments.iter().position(|s| s.start.0 > f.0))
}

/// Set the in-point of the segment under the playhead.
pub fn set_in(segments: &mut Vec<Segment>, playhead: FrameIdx) {
    let Some(i) = segment_at_or_after(segments, playhead) else {
        return;
    };
    let end = segments[i].end;
    if playhead.0 < end.0 {
        segments[i].start = playhead;
    }
    prune(segments);
}

/// Set the out-point of the segment under the playhead. The playhead frame is
/// kept, so `end` is one past it.
pub fn set_out(segments: &mut Vec<Segment>, playhead: FrameIdx) {
    let Some(i) = segment_at(segments, playhead)
        .or_else(|| segments.iter().rposition(|s| s.end.0 <= playhead.0))
    else {
        return;
    };
    let start = segments[i].start;
    let end = FrameIdx(playhead.0 + 1);
    if end.0 > start.0 {
        segments[i].end = end;
    }
    prune(segments);
}

/// Remove `[from, to)` from the kept ranges, splitting a segment in two if the
/// range falls inside one. This is the cut.
pub fn delete_range(segments: &mut Vec<Segment>, from: FrameIdx, to: FrameIdx) {
    if to.0 <= from.0 {
        return;
    }
    let mut out: Vec<Segment> = Vec::with_capacity(segments.len() + 1);
    for s in segments.iter().copied() {
        // No overlap.
        if s.end.0 <= from.0 || s.start.0 >= to.0 {
            out.push(s);
            continue;
        }
        if s.start.0 < from.0 {
            out.push(Segment {
                start: s.start,
                end: from,
            });
        }
        if s.end.0 > to.0 {
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
pub fn keep_only(segments: &mut Vec<Segment>, from: FrameIdx, to: FrameIdx) {
    let mut out: Vec<Segment> = Vec::new();
    for s in segments.iter().copied() {
        let start = FrameIdx(s.start.0.max(from.0));
        let end = FrameIdx(s.end.0.min(to.0));
        if end.0 > start.0 {
            out.push(Segment { start, end });
        }
    }
    *segments = out;
    prune(segments);
}

/// Split the segment under the playhead in two. Both halves are kept; this is
/// how a cut point gets created before deleting one side.
pub fn split_at(segments: &mut Vec<Segment>, playhead: FrameIdx) {
    let Some(i) = segment_at(segments, playhead) else {
        return;
    };
    let s = segments[i];
    if playhead.0 <= s.start.0 || playhead.0 >= s.end.0 {
        return;
    }
    segments[i] = Segment {
        start: s.start,
        end: playhead,
    };
    segments.insert(
        i + 1,
        Segment {
            start: playhead,
            end: s.end,
        },
    );
}

/// Drop empty segments, keeping order.
fn prune(segments: &mut Vec<Segment>) {
    segments.retain(|s| s.end.0 > s.start.0);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn segs(v: &[(u64, u64)]) -> Vec<Segment> {
        v.iter()
            .map(|(a, b)| Segment {
                start: FrameIdx(*a),
                end: FrameIdx(*b),
            })
            .collect()
    }

    #[test]
    fn in_and_out_trim_the_segment_under_the_playhead() {
        let mut s = segs(&[(0, 1000)]);
        set_in(&mut s, FrameIdx(100));
        assert_eq!(s, segs(&[(100, 1000)]));
        set_out(&mut s, FrameIdx(499));
        assert_eq!(s, segs(&[(100, 500)]));
    }

    #[test]
    fn delete_inside_a_segment_splits_it() {
        let mut s = segs(&[(0, 1000)]);
        delete_range(&mut s, FrameIdx(200), FrameIdx(300));
        assert_eq!(s, segs(&[(0, 200), (300, 1000)]));
    }

    #[test]
    fn delete_spanning_segments_trims_both_ends() {
        let mut s = segs(&[(0, 200), (300, 1000)]);
        delete_range(&mut s, FrameIdx(100), FrameIdx(400));
        assert_eq!(s, segs(&[(0, 100), (400, 1000)]));
    }

    #[test]
    fn delete_covering_a_whole_segment_removes_it() {
        let mut s = segs(&[(0, 200), (300, 400), (500, 600)]);
        delete_range(&mut s, FrameIdx(250), FrameIdx(450));
        assert_eq!(s, segs(&[(0, 200), (500, 600)]));
    }

    #[test]
    fn delete_outside_everything_is_a_no_op() {
        let mut s = segs(&[(0, 200)]);
        delete_range(&mut s, FrameIdx(500), FrameIdx(600));
        delete_range(&mut s, FrameIdx(50), FrameIdx(50));
        assert_eq!(s, segs(&[(0, 200)]));
    }

    #[test]
    fn split_creates_two_adjacent_segments() {
        let mut s = segs(&[(0, 1000)]);
        split_at(&mut s, FrameIdx(400));
        assert_eq!(s, segs(&[(0, 400), (400, 1000)]));
        // Splitting on a boundary changes nothing.
        split_at(&mut s, FrameIdx(400));
        assert_eq!(s, segs(&[(0, 400), (400, 1000)]));
    }

    #[test]
    fn keep_only_clips_every_segment() {
        let mut s = segs(&[(0, 200), (300, 1000)]);
        keep_only(&mut s, FrameIdx(150), FrameIdx(400));
        assert_eq!(s, segs(&[(150, 200), (300, 400)]));
    }

    #[test]
    fn in_point_past_the_end_is_ignored() {
        let mut s = segs(&[(0, 100)]);
        set_in(&mut s, FrameIdx(500));
        assert_eq!(s, segs(&[(0, 100)]));
    }
}
