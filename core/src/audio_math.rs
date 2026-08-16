//! Gain maths shared by the live preview mixer and the ffmpeg export chain.
//! One implementation, both callers — if these two ever disagree the preview
//! is lying about what export will produce.

/// Below this the UI calls it silence and the export path drops the input.
pub const MUTE_DB: f32 = -60.0;

/// The limiter ceiling used at the end of the mix, in linear amplitude.
/// `amix normalize=0` lets the sum clip, so this is not optional.
pub const LIMITER_CEILING: f32 = 0.95;

/// How long an added track's fade-out lasts.
///
/// Seconds, not samples: `command` renders this straight into an `afade`
/// argument, and the preview mixer converts once against `PCM_SAMPLE_RATE`.
/// Exposing both units from here would put the conversion in the crate that
/// has no business knowing the mixer's sample rate, and the mixer already owns
/// a `secs_to_samples` for exactly this.
pub const FADE_OUT_SECS: f64 = 2.0;

pub fn db_to_linear(db: f32) -> f32 {
    if db <= MUTE_DB || db == f32::NEG_INFINITY {
        0.0
    } else {
        10f32.powf(db / 20.0)
    }
}

pub fn linear_to_db(linear: f32) -> f32 {
    if linear <= 0.0 {
        f32::NEG_INFINITY
    } else {
        20.0 * linear.log10()
    }
}

pub fn is_muted(db: f32) -> bool {
    db <= MUTE_DB || db == f32::NEG_INFINITY
}

/// Linear gain formatted for a `volume=` filter argument. Fixed precision so
/// golden tests on argument vectors stay stable.
pub fn linear_arg(db: f32) -> String {
    format!("{:.4}", db_to_linear(db))
}

/// dB as shown in the UI.
pub fn format_db(db: f32) -> String {
    if is_muted(db) {
        "muted".to_string()
    } else if db > 0.0 {
        format!("+{db:.1} dB")
    } else {
        format!("{db:.1} dB")
    }
}

/// Soft limiter matching `alimiter=limit=LIMITER_CEILING` closely enough that
/// the preview does not sound different from the export. Hard-clips at the
/// ceiling with a knee, which is what alimiter does for steady-state material.
pub fn limit_sample(x: f32) -> f32 {
    const KNEE: f32 = 0.05;
    let ceil = LIMITER_CEILING;
    let knee_start = ceil - KNEE;
    let a = x.abs();
    if a <= knee_start {
        x
    } else {
        let over = (a - knee_start) / (KNEE + (a - ceil).max(0.0) + KNEE);
        let shaped = knee_start + KNEE * (1.0 - (-over).exp());
        shaped.min(ceil) * x.signum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_db_is_unity() {
        assert!((db_to_linear(0.0) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn six_db_is_about_double() {
        assert!((db_to_linear(6.0) - 1.9953).abs() < 1e-3);
        assert!((db_to_linear(-6.0) - 0.5012).abs() < 1e-3);
    }

    #[test]
    fn roundtrip() {
        for db in [-40.0f32, -12.0, -3.0, 0.0, 3.5, 12.0] {
            let back = linear_to_db(db_to_linear(db));
            assert!((back - db).abs() < 1e-3, "{db} -> {back}");
        }
    }

    #[test]
    fn mute_floor() {
        assert_eq!(db_to_linear(MUTE_DB), 0.0);
        assert_eq!(db_to_linear(f32::NEG_INFINITY), 0.0);
        assert!(is_muted(-80.0));
        assert!(!is_muted(-59.0));
    }

    #[test]
    fn limiter_never_exceeds_ceiling() {
        for i in 0..2000 {
            let x = i as f32 / 100.0 - 10.0;
            assert!(limit_sample(x).abs() <= LIMITER_CEILING + 1e-6, "x={x}");
        }
    }

    #[test]
    fn limiter_is_transparent_below_the_knee() {
        assert_eq!(limit_sample(0.5), 0.5);
        assert_eq!(limit_sample(-0.5), -0.5);
    }

    #[test]
    fn linear_arg_is_stable() {
        assert_eq!(linear_arg(0.0), "1.0000");
        assert_eq!(linear_arg(-6.0), "0.5012");
        assert_eq!(linear_arg(f32::NEG_INFINITY), "0.0000");
    }
}
