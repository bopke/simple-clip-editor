//! Exact rational time. Edit points never touch `f64`.
//!
//! # Where `f64` is and is not allowed
//!
//! The rule is about *what the number becomes*, not about which file it is in.
//! A cast is a bug when its result is stored as, or decides, an edit point:
//! a segment boundary, the playhead, the selection, the crop rectangle. Those
//! go through [`Rational`]/[`RationalTime`] — [`FrameIdx::to_time`] and
//! [`FrameIdx::from_time_round`] convert between a frame on some grid and the
//! instant it names, exactly, at any rate.
//!
//! Three kinds of `f64`/`as` remain deliberately, and are not worth converting:
//!
//! - **Pixel geometry** (`crop.rs`, `model::pad_box`, `scale_to_long_edge`).
//!   A pointer position is a float by nature and the result is quantised to
//!   whole even pixels before it is stored. There is no exact value being lost.
//! - **UI layout** (`ui_timeline`, `ui_preview`). Frames map to and from screen
//!   x-coordinates; the float is the screen, and the frame index that comes
//!   back out is an integer.
//! - **Audio sample maths** (`playback.rs`, `audio.rs`, `audio_math.rs`).
//!   Samples are `f32` all the way to the device, and the mixer's arithmetic is
//!   over sample values rather than over positions in the edit.
//!
//! The distinction that matters: rounding a *display* is discarded on the next
//! frame, while rounding an *edit point* is written into the project and
//! compounds every time it happens again.

use serde::{Deserialize, Serialize};
use std::fmt;

fn gcd(a: i64, b: i64) -> i64 {
    let (mut a, mut b) = (a.abs(), b.abs());
    while b != 0 {
        let t = b;
        b = a % b;
        a = t;
    }
    if a == 0 {
        1
    } else {
        a
    }
}

/// The largest magnitude a `num` or `den` may have when it arrives from a file.
///
/// Arithmetic here multiplies numerators by denominators in `i64`, so a pair of
/// legitimate-looking but enormous values overflows — a panic in a debug build
/// and a nonsense edit point in a release one. `2^50` is ~35 years at
/// microsecond resolution, which is past anything anyone will trim.
const MAX_MAGNITUDE: i64 = 1 << 50;

/// A rational as it appears on disk, before any invariant has been checked.
///
/// Deserialising straight into [`Rational`] would accept `0/0`, which parses
/// fine and then divides by zero the first time it is used as a frame rate.
#[derive(Deserialize)]
struct RationalRepr {
    num: i64,
    den: i64,
}

impl RationalRepr {
    /// The invariants every constructor upholds: positive denominator, reduced
    /// to lowest terms, small enough to do arithmetic with. Every value written
    /// by this program satisfies these, so rejecting rather than repairing
    /// keeps a single representation per value — which is what lets the derived
    /// `PartialEq` agree with [`RationalTime::cmp_time`].
    fn checked(self) -> Result<(i64, i64), String> {
        let (num, den) = (self.num, self.den);
        if den == 0 {
            return Err(format!("{num}/{den} has a zero denominator"));
        }
        if num.abs() > MAX_MAGNITUDE || den.abs() > MAX_MAGNITUDE {
            return Err(format!("{num}/{den} is too large to be a time or a rate"));
        }
        if den < 0 || gcd(num, den) != 1 {
            return Err(format!("{num}/{den} is not in lowest terms"));
        }
        Ok((num, den))
    }
}

/// A rational number, kept normalised with a positive denominator.
#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "RationalRepr")]
pub struct Rational {
    pub num: i64,
    pub den: i64,
}

impl TryFrom<RationalRepr> for Rational {
    type Error = String;
    fn try_from(repr: RationalRepr) -> Result<Rational, String> {
        let (num, den) = repr.checked()?;
        Ok(Rational { num, den })
    }
}

impl Default for Rational {
    fn default() -> Self {
        Rational::ZERO
    }
}

impl Default for RationalTime {
    fn default() -> Self {
        RationalTime::ZERO
    }
}

impl Rational {
    pub const ZERO: Rational = Rational { num: 0, den: 1 };

    pub fn new(num: i64, den: i64) -> Rational {
        assert!(den != 0, "rational with zero denominator");
        let sign = if den < 0 { -1 } else { 1 };
        let g = gcd(num, den);
        Rational {
            num: sign * num / g,
            den: sign * den / g,
        }
    }

    pub fn from_int(n: i64) -> Rational {
        Rational { num: n, den: 1 }
    }

    /// Parse ffprobe's `num/den` form. Returns `None` for `0/0`, which ffprobe
    /// emits when it has no idea (audio-only streams, some containers).
    pub fn parse_ffprobe(s: &str) -> Option<Rational> {
        let (n, d) = s.split_once('/')?;
        let n: i64 = n.trim().parse().ok()?;
        let d: i64 = d.trim().parse().ok()?;
        if d == 0 || n == 0 {
            return None;
        }
        Some(Rational::new(n, d))
    }

    pub fn is_zero(self) -> bool {
        self.num == 0
    }

    pub fn inv(self) -> Rational {
        Rational::new(self.den, self.num)
    }

    pub fn as_f64(self) -> f64 {
        self.num as f64 / self.den as f64
    }

    /// Snap a measured frame rate to a common broadcast value. OBS reports
    /// things like `60000/1001` or `59.94001`; the project timeline wants the
    /// nearest sane rate.
    pub fn snap_to_common_fps(self) -> Rational {
        const COMMON: &[Rational] = &[
            Rational {
                num: 24000,
                den: 1001,
            },
            Rational { num: 24, den: 1 },
            Rational { num: 25, den: 1 },
            Rational {
                num: 30000,
                den: 1001,
            },
            Rational { num: 30, den: 1 },
            Rational { num: 48, den: 1 },
            Rational { num: 50, den: 1 },
            Rational {
                num: 60000,
                den: 1001,
            },
            Rational { num: 60, den: 1 },
            Rational { num: 120, den: 1 },
            Rational { num: 144, den: 1 },
            Rational { num: 240, den: 1 },
        ];
        let v = self.as_f64();
        let mut best = self;
        let mut best_err = f64::INFINITY;
        for c in COMMON {
            let err = (c.as_f64() - v).abs();
            if err < best_err {
                best_err = err;
                best = *c;
            }
        }
        // Only snap if we are within 1% — an exotic but deliberate rate stays.
        if best_err <= v * 0.01 {
            best
        } else {
            self
        }
    }

    /// How a frame rate is written for a human: 30, 29.97, 23.976. The value
    /// itself stays rational — this is a label, never a round-trip.
    pub fn to_display_fps(self) -> String {
        if self.den == 1 {
            return self.num.to_string();
        }
        let text = format!("{:.3}", self.as_f64());
        let text = text.trim_end_matches('0').trim_end_matches('.');
        text.to_string()
    }

    /// `-r`/`fps=` argument form. Rational rates stay rational so 59.94 does
    /// not become 59.94000244140625.
    pub fn to_ffmpeg_arg(self) -> String {
        if self.den == 1 {
            self.num.to_string()
        } else {
            format!("{}/{}", self.num, self.den)
        }
    }
}

impl std::ops::Mul for Rational {
    type Output = Rational;
    fn mul(self, other: Rational) -> Rational {
        Rational::new(self.num * other.num, self.den * other.den)
    }
}

impl fmt::Debug for Rational {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.num, self.den)
    }
}

impl fmt::Display for Rational {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.den == 1 {
            write!(f, "{}", self.num)
        } else {
            write!(f, "{:.3}", self.as_f64())
        }
    }
}

/// Exact time in seconds. Never use `f64` for edit points.
#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "RationalRepr")]
pub struct RationalTime {
    pub num: i64,
    pub den: i64,
}

impl TryFrom<RationalRepr> for RationalTime {
    type Error = String;
    fn try_from(repr: RationalRepr) -> Result<RationalTime, String> {
        let (num, den) = repr.checked()?;
        Ok(RationalTime { num, den })
    }
}

impl RationalTime {
    pub const ZERO: RationalTime = RationalTime { num: 0, den: 1 };

    pub fn new(num: i64, den: i64) -> RationalTime {
        let r = Rational::new(num, den);
        RationalTime {
            num: r.num,
            den: r.den,
        }
    }

    pub fn from_seconds_f64(s: f64) -> RationalTime {
        // Only ever used for values that arrived as float in the first place
        // (ffprobe durations), never for edit points.
        RationalTime::new((s * 1_000_000.0).round() as i64, 1_000_000)
    }

    pub fn from_millis(ms: i64) -> RationalTime {
        RationalTime::new(ms, 1000)
    }

    pub fn as_f64(self) -> f64 {
        self.num as f64 / self.den as f64
    }

    pub fn is_negative(self) -> bool {
        self.num < 0
    }

    pub fn cmp_time(self, other: RationalTime) -> std::cmp::Ordering {
        (self.num as i128 * other.den as i128).cmp(&(other.num as i128 * self.den as i128))
    }

    /// Whole milliseconds, rounded. `adelay` wants integers.
    pub fn as_millis_round(self) -> i64 {
        let n = self.num as i128 * 1000;
        let d = self.den as i128;
        let half = d / 2;
        let r = if n >= 0 {
            (n + half) / d
        } else {
            (n - half) / d
        };
        r as i64
    }

    /// Exact decimal seconds with microsecond resolution, for `-ss`/`-to`.
    /// Integer maths throughout: no float ever sees an edit point.
    pub fn to_ffmpeg_arg(self) -> String {
        let neg = self.num < 0;
        let n = (self.num as i128).abs();
        let d = self.den as i128;
        let whole = n / d;
        let frac_us = ((n % d) * 1_000_000 + d / 2) / d;
        // Rounding the fraction can carry into the whole part.
        let (whole, frac_us) = if frac_us >= 1_000_000 {
            (whole + 1, 0)
        } else {
            (whole, frac_us)
        };
        let mut s = String::new();
        if neg {
            s.push('-');
        }
        s.push_str(&whole.to_string());
        s.push('.');
        s.push_str(&format!("{:06}", frac_us));
        while s.ends_with('0') {
            s.pop();
        }
        if s.ends_with('.') {
            s.push('0');
        }
        s
    }

    /// `H:MM:SS.mmm` for the UI.
    pub fn to_display(self) -> String {
        let neg = self.num < 0;
        let total_ms = self.neg_abs_millis();
        let ms = total_ms % 1000;
        let total_s = total_ms / 1000;
        let s = total_s % 60;
        let m = (total_s / 60) % 60;
        let h = total_s / 3600;
        let sign = if neg { "-" } else { "" };
        if h > 0 {
            format!("{sign}{h}:{m:02}:{s:02}.{ms:03}")
        } else {
            format!("{sign}{m:02}:{s:02}.{ms:03}")
        }
    }

    fn neg_abs_millis(self) -> i64 {
        let t = RationalTime {
            num: self.num.abs(),
            den: self.den,
        };
        t.as_millis_round()
    }
}

/// Ordering by value, not by representation. `new` normalises, so every
/// `RationalTime` in the model has a unique `(num, den)` and the derived
/// `PartialEq` agrees with this comparison rather than contradicting it.
impl Ord for RationalTime {
    fn cmp(&self, other: &RationalTime) -> std::cmp::Ordering {
        self.cmp_time(*other)
    }
}

impl PartialOrd for RationalTime {
    fn partial_cmp(&self, other: &RationalTime) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl std::ops::Neg for RationalTime {
    type Output = RationalTime;
    fn neg(self) -> RationalTime {
        RationalTime {
            num: -self.num,
            den: self.den,
        }
    }
}

impl std::ops::Add for RationalTime {
    type Output = RationalTime;
    fn add(self, other: RationalTime) -> RationalTime {
        RationalTime::new(
            self.num * other.den + other.num * self.den,
            self.den * other.den,
        )
    }
}

impl std::ops::Sub for RationalTime {
    type Output = RationalTime;
    fn sub(self, other: RationalTime) -> RationalTime {
        self + (-other)
    }
}

impl fmt::Debug for RationalTime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}s", self.to_ffmpeg_arg())
    }
}

/// Frame index on the project timeline.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Serialize, Deserialize, Default)]
pub struct FrameIdx(pub u64);

impl FrameIdx {
    /// Timeline time of this frame's start at `fps`.
    pub fn to_time(self, fps: Rational) -> RationalTime {
        // frames / fps  =  frames * fps.den / fps.num
        RationalTime::new(self.0 as i64 * fps.den, fps.num)
    }

    /// Frame containing `t`, truncating.
    pub fn from_time(t: RationalTime, fps: Rational) -> FrameIdx {
        if t.num <= 0 {
            return FrameIdx(0);
        }
        let n = t.num as i128 * fps.num as i128;
        let d = t.den as i128 * fps.den as i128;
        FrameIdx((n / d) as u64)
    }

    /// Nearest frame to `t`, rounding.
    pub fn from_time_round(t: RationalTime, fps: Rational) -> FrameIdx {
        if t.num <= 0 {
            return FrameIdx(0);
        }
        let n = t.num as i128 * fps.num as i128;
        let d = t.den as i128 * fps.den as i128;
        FrameIdx(((n + d / 2) / d) as u64)
    }

    pub fn saturating_add(self, n: u64) -> FrameIdx {
        FrameIdx(self.0.saturating_add(n))
    }

    pub fn saturating_sub(self, n: u64) -> FrameIdx {
        FrameIdx(self.0.saturating_sub(n))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `0/0` is valid JSON and, before this, a valid `Rational` — which then
    /// divided by zero the first time it was used as a frame rate. Validation
    /// belongs at the edge, not at every use.
    #[test]
    fn a_degenerate_rational_is_refused_at_the_edge() {
        let bad = |s: &str| serde_json::from_str::<Rational>(s).unwrap_err().to_string();
        assert!(bad(r#"{"num":0,"den":0}"#).contains("zero denominator"));
        assert!(bad(r#"{"num":30,"den":0}"#).contains("zero denominator"));
        // Two ways of writing 1/2 would break the derived PartialEq.
        assert!(bad(r#"{"num":2,"den":4}"#).contains("lowest terms"));
        assert!(bad(r#"{"num":-1,"den":-2}"#).contains("lowest terms"));
        // Big enough to overflow the i64 arithmetic downstream.
        assert!(bad(r#"{"num":9223372036854775807,"den":1}"#).contains("too large"));

        let good: Rational = serde_json::from_str(r#"{"num":30000,"den":1001}"#).unwrap();
        assert_eq!(good, Rational::new(30000, 1001));
        let t: RationalTime = serde_json::from_str(r#"{"num":-3,"den":2}"#).unwrap();
        assert_eq!(t, RationalTime::new(-3, 2));
        assert!(serde_json::from_str::<RationalTime>(r#"{"num":1,"den":0}"#).is_err());
    }

    /// Everything the model can produce survives the round trip the check
    /// above is strict about.
    #[test]
    fn normalised_values_round_trip_through_serde() {
        let values = [
            Rational::ZERO,
            Rational::from_int(60),
            Rational::new(30000, 1001),
            Rational::new(4, -8),
            Rational::new(0, 5),
        ];
        for v in values {
            let text = serde_json::to_string(&v).unwrap();
            assert_eq!(serde_json::from_str::<Rational>(&text).unwrap(), v, "{v:?}");
        }
        for t in [
            RationalTime::ZERO,
            RationalTime::new(3607, 2),
            RationalTime::from_millis(-1500),
            RationalTime::from_seconds_f64(90.125),
        ] {
            let text = serde_json::to_string(&t).unwrap();
            assert_eq!(
                serde_json::from_str::<RationalTime>(&text).unwrap(),
                t,
                "{t:?}"
            );
        }
    }

    #[test]
    fn rational_normalises() {
        let r = Rational::new(60000, 2002);
        assert_eq!((r.num, r.den), (30000, 1001));
        let r = Rational::new(4, -8);
        assert_eq!((r.num, r.den), (-1, 2));
    }

    #[test]
    fn parses_ffprobe_rates() {
        assert_eq!(
            Rational::parse_ffprobe("60000/1001"),
            Some(Rational::new(60000, 1001))
        );
        assert_eq!(Rational::parse_ffprobe("0/0"), None);
        assert_eq!(Rational::parse_ffprobe("nonsense"), None);
    }

    #[test]
    fn frame_rates_read_as_decimals() {
        assert_eq!(Rational::from_int(60).to_display_fps(), "60");
        assert_eq!(Rational::new(30000, 1001).to_display_fps(), "29.97");
        assert_eq!(Rational::new(60000, 1001).to_display_fps(), "59.94");
        assert_eq!(Rational::new(24000, 1001).to_display_fps(), "23.976");
        // The label changes; what gets passed to ffmpeg does not.
        assert_eq!(Rational::new(30000, 1001).to_ffmpeg_arg(), "30000/1001");
    }

    #[test]
    fn snaps_fps() {
        assert_eq!(
            Rational::new(5994, 100).snap_to_common_fps(),
            Rational::new(60000, 1001)
        );
        assert_eq!(
            Rational::new(3000000, 100001).snap_to_common_fps(),
            Rational::new(30, 1)
        );
        // An exotic rate stays put rather than being snapped to nonsense.
        assert_eq!(
            Rational::new(7, 1).snap_to_common_fps(),
            Rational::new(7, 1)
        );
    }

    #[test]
    fn frame_time_roundtrip_is_exact_at_ntsc_rates() {
        let fps = Rational::new(60000, 1001);
        for f in [0u64, 1, 59, 60, 3600, 123_456] {
            let t = FrameIdx(f).to_time(fps);
            assert_eq!(FrameIdx::from_time(t, fps), FrameIdx(f), "frame {f}");
        }
    }

    #[test]
    fn ffmpeg_time_args_are_exact_decimals() {
        assert_eq!(RationalTime::new(0, 1).to_ffmpeg_arg(), "0.0");
        assert_eq!(RationalTime::new(1, 2).to_ffmpeg_arg(), "0.5");
        assert_eq!(RationalTime::new(1001, 30000).to_ffmpeg_arg(), "0.033367");
        assert_eq!(RationalTime::new(-3, 2).to_ffmpeg_arg(), "-1.5");
        assert_eq!(RationalTime::new(90, 1).to_ffmpeg_arg(), "90.0");
    }

    #[test]
    fn millis_rounding_is_symmetric() {
        assert_eq!(RationalTime::new(1, 3).as_millis_round(), 333);
        assert_eq!(RationalTime::new(2, 3).as_millis_round(), 667);
        assert_eq!(RationalTime::new(-2, 3).as_millis_round(), -667);
    }

    #[test]
    fn display_format() {
        assert_eq!(RationalTime::new(3661, 1).to_display(), "1:01:01.000");
        assert_eq!(RationalTime::new(5, 2).to_display(), "00:02.500");
    }
}
