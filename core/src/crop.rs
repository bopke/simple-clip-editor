//! Crop rectangle geometry: ratio locking, bounds clamping, even snapping.
//!
//! Snapping happens here, on every drag update, not at export time — what the
//! user sees in the dimension readout must be what they get.

use crate::model::{even, CropRect};
use crate::time::Rational;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Handle {
    Move,
    N,
    S,
    E,
    W,
    NE,
    NW,
    SE,
    SW,
}

impl Handle {
    pub fn all_resize() -> [Handle; 8] {
        [
            Handle::NW,
            Handle::N,
            Handle::NE,
            Handle::E,
            Handle::SE,
            Handle::S,
            Handle::SW,
            Handle::W,
        ]
    }

    fn moves_left(self) -> bool {
        matches!(self, Handle::W | Handle::NW | Handle::SW)
    }
    fn moves_right(self) -> bool {
        matches!(self, Handle::E | Handle::NE | Handle::SE)
    }
    fn moves_top(self) -> bool {
        matches!(self, Handle::N | Handle::NE | Handle::NW)
    }
    fn moves_bottom(self) -> bool {
        matches!(self, Handle::S | Handle::SE | Handle::SW)
    }
}

/// Minimum crop size in source pixels. Small enough not to get in the way,
/// large enough that the encoder never sees a degenerate frame.
pub const MIN_SIZE: u32 = 16;

/// Largest rect of `ratio` centred in `bounds`.
pub fn centered_with_ratio(bounds: (u32, u32), ratio: Rational) -> CropRect {
    let (bw, bh) = (bounds.0 as f64, bounds.1 as f64);
    let r = ratio.as_f64();
    let (w, h) = if bw / bh > r {
        (bh * r, bh)
    } else {
        (bw, bw / r)
    };
    let w = even(w.round() as u32).min(even(bounds.0));
    let h = even(h.round() as u32).min(even(bounds.1));
    CropRect {
        x: even_pos(bounds.0.saturating_sub(w) / 2),
        y: even_pos(bounds.1.saturating_sub(h) / 2),
        w,
        h,
    }
}

pub fn full_frame(bounds: (u32, u32)) -> CropRect {
    CropRect {
        x: 0,
        y: 0,
        w: even(bounds.0),
        h: even(bounds.1),
    }
}

/// Move the whole rect by a delta, clamped inside `bounds`.
pub fn translate(rect: CropRect, dx: f32, dy: f32, bounds: (u32, u32)) -> CropRect {
    let max_x = bounds.0.saturating_sub(rect.w) as f32;
    let max_y = bounds.1.saturating_sub(rect.h) as f32;
    CropRect {
        x: even_pos((rect.x as f32 + dx).clamp(0.0, max_x).round() as u32),
        y: even_pos((rect.y as f32 + dy).clamp(0.0, max_y).round() as u32),
        ..rect
    }
}

/// Drag `handle` to source-pixel position `(px, py)`.
///
/// `ratio` is `None` for free dragging — which is also what Shift produces,
/// since Shift temporarily unlocks the ratio.
pub fn drag_handle(
    rect: CropRect,
    handle: Handle,
    px: f32,
    py: f32,
    ratio: Option<Rational>,
    bounds: (u32, u32),
) -> CropRect {
    if handle == Handle::Move {
        return rect;
    }
    let (bw, bh) = (bounds.0 as f64, bounds.1 as f64);
    let (mut l, mut t) = (rect.x as f64, rect.y as f64);
    let (mut r, mut b) = ((rect.x + rect.w) as f64, (rect.y + rect.h) as f64);
    let px = (px as f64).clamp(0.0, bw);
    let py = (py as f64).clamp(0.0, bh);
    let min = MIN_SIZE as f64;

    if handle.moves_left() {
        l = px.min(r - min);
    }
    if handle.moves_right() {
        r = px.max(l + min);
    }
    if handle.moves_top() {
        t = py.min(b - min);
    }
    if handle.moves_bottom() {
        b = py.max(t + min);
    }

    if let Some(ratio) = ratio {
        let target = ratio.as_f64();
        let mut w = r - l;
        let mut h = b - t;
        // Anchor the edge the user is not dragging.
        let anchor_x = if handle.moves_left() { r } else { l };
        let anchor_y = if handle.moves_top() { b } else { t };

        match handle {
            Handle::E | Handle::W => h = w / target,
            Handle::N | Handle::S => w = h * target,
            _ => {
                // Corner: drive by whichever axis the pointer pushed further.
                if (w / target) >= h {
                    h = w / target;
                } else {
                    w = h * target;
                }
            }
        }

        // Shrink to fit the frame, keeping the ratio.
        let max_w = if handle.moves_left() {
            anchor_x
        } else if handle.moves_right() {
            bw - anchor_x
        } else {
            bw
        };
        let max_h = if handle.moves_top() {
            anchor_y
        } else if handle.moves_bottom() {
            bh - anchor_y
        } else {
            bh
        };
        let scale = (max_w / w).min(max_h / h).min(1.0);
        w *= scale;
        h *= scale;

        // Place against the anchors; edge handles grow symmetrically on the
        // free axis so the rect does not walk sideways.
        if handle.moves_left() {
            l = anchor_x - w;
            r = anchor_x;
        } else if handle.moves_right() {
            l = anchor_x;
            r = anchor_x + w;
        } else {
            let cx = (l + r) / 2.0;
            l = (cx - w / 2.0).clamp(0.0, bw - w);
            r = l + w;
        }
        if handle.moves_top() {
            t = anchor_y - h;
            b = anchor_y;
        } else if handle.moves_bottom() {
            t = anchor_y;
            b = anchor_y + h;
        } else {
            let cy = (t + b) / 2.0;
            t = (cy - h / 2.0).clamp(0.0, bh - h);
            b = t + h;
        }
    }

    finalise(l, t, r, b, bounds)
}

/// Re-fit an existing rect to a newly chosen ratio, keeping its centre and
/// staying inside the frame.
pub fn apply_ratio(rect: CropRect, ratio: Rational, bounds: (u32, u32)) -> CropRect {
    let target = ratio.as_f64();
    let (bw, bh) = (bounds.0 as f64, bounds.1 as f64);
    let cx = rect.x as f64 + rect.w as f64 / 2.0;
    let cy = rect.y as f64 + rect.h as f64 / 2.0;
    let area_w = rect.w as f64;
    let area_h = rect.h as f64;
    let (mut w, mut h) = if area_w / area_h > target {
        (area_h * target, area_h)
    } else {
        (area_w, area_w / target)
    };
    let scale = (bw / w).min(bh / h).min(1.0);
    w *= scale;
    h *= scale;
    let l = (cx - w / 2.0).clamp(0.0, bw - w);
    let t = (cy - h / 2.0).clamp(0.0, bh - h);
    finalise(l, t, l + w, t + h, bounds)
}

/// Even value for a *position*. `even()` has a floor of 2 because a dimension
/// of zero is nonsense; an offset of zero is not.
pub fn even_pos(v: u32) -> u32 {
    v & !1
}

fn finalise(l: f64, t: f64, r: f64, b: f64, bounds: (u32, u32)) -> CropRect {
    fn axis(lo: f64, hi: f64, limit: u32) -> (u32, u32) {
        let limit = even(limit);
        let (lo, hi) = (lo.min(hi), hi.max(lo));
        let lo = lo.clamp(0.0, limit as f64).round() as u32;
        let hi = hi.clamp(0.0, limit as f64).round() as u32;
        let pos = even_pos(lo).min(limit.saturating_sub(MIN_SIZE));
        let mut size = even(hi.saturating_sub(pos)).max(MIN_SIZE.min(limit));
        if pos + size > limit {
            size = even(limit - pos);
        }
        (pos, size)
    }
    let (x, w) = axis(l, r, bounds.0);
    let (y, h) = axis(t, b, bounds.1);
    CropRect { x, y, w, h }
}

/// Text for the live dimension readout: "1080 × 1920  (9:16)".
pub fn describe(rect: CropRect) -> String {
    format!("{} × {}  ({})", rect.w, rect.h, ratio_name(rect))
}

fn ratio_name(rect: CropRect) -> String {
    let r = rect.ratio();
    for (name, preset) in crate::model::ASPECT_PRESETS {
        if (preset.as_f64() as f32 - r).abs() < 0.005 {
            return (*name).to_string();
        }
    }
    format!("{r:.3}:1")
}

#[cfg(test)]
mod tests {
    use super::*;

    const B: (u32, u32) = (1920, 1080);

    #[test]
    fn everything_is_even() {
        let r = drag_handle(full_frame(B), Handle::SE, 333.0, 777.0, None, B);
        assert_eq!(r.w % 2, 0);
        assert_eq!(r.h % 2, 0);
        assert_eq!(r.x % 2, 0);
        assert_eq!(r.y % 2, 0);
    }

    #[test]
    fn centered_9_16_in_1080p() {
        let r = centered_with_ratio(B, Rational::new(9, 16));
        assert_eq!((r.w, r.h), (608, 1080));
        assert_eq!(r.x, 656);
        assert_eq!(r.y, 0);
    }

    #[test]
    fn locked_drag_keeps_ratio() {
        let start = centered_with_ratio(B, Rational::new(1, 1));
        let r = drag_handle(
            start,
            Handle::SE,
            1900.0,
            1000.0,
            Some(Rational::new(1, 1)),
            B,
        );
        assert!((r.w as i64 - r.h as i64).abs() <= 2, "{r:?}");
        assert!(r.x + r.w <= B.0 && r.y + r.h <= B.1, "{r:?}");
    }

    #[test]
    fn drag_never_leaves_the_frame() {
        for h in Handle::all_resize() {
            for (px, py) in [(-500.0, -500.0), (5000.0, 5000.0), (0.0, 5000.0)] {
                let r = drag_handle(full_frame(B), h, px, py, None, B);
                assert!(r.x + r.w <= B.0, "{h:?} {r:?}");
                assert!(r.y + r.h <= B.1, "{h:?} {r:?}");
                assert!(r.w >= 2 && r.h >= 2, "{h:?} {r:?}");
            }
        }
    }

    #[test]
    fn locked_drag_never_leaves_the_frame() {
        let ratio = Rational::new(9, 16);
        for h in Handle::all_resize() {
            let start = centered_with_ratio(B, ratio);
            let r = drag_handle(start, h, 4000.0, 4000.0, Some(ratio), B);
            assert!(r.x + r.w <= B.0, "{h:?} {r:?}");
            assert!(r.y + r.h <= B.1, "{h:?} {r:?}");
        }
    }

    #[test]
    fn translate_clamps() {
        let r = centered_with_ratio(B, Rational::new(1, 1));
        let moved = translate(r, -9999.0, -9999.0, B);
        assert_eq!((moved.x, moved.y), (0, 0));
        let moved = translate(r, 9999.0, 9999.0, B);
        assert_eq!(moved.x + moved.w, B.0);
    }

    #[test]
    fn apply_ratio_fits_inside() {
        let r = apply_ratio(full_frame(B), Rational::new(9, 16), B);
        assert_eq!((r.w, r.h), (608, 1080));
    }

    #[test]
    fn describe_names_known_ratios() {
        assert_eq!(
            describe(CropRect {
                x: 0,
                y: 0,
                w: 1080,
                h: 1920
            }),
            "1080 × 1920  (9:16)"
        );
    }
}
