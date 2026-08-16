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

/// Soft limiter holding the preview to [`LIMITER_CEILING`], the same ceiling
/// the export's `alimiter` is given.
///
/// # Where this and the export agree, and where they do not
///
/// The ceiling is the same number and steady-state material comes out at the
/// same level, which is what makes the preview usable for judging gain. The
/// dynamics are **not** the same and cannot be from here: `alimiter` is a
/// look-ahead limiter with attack (5 ms) and release (50 ms) defaults, so it
/// starts ducking before a transient arrives and recovers gradually
/// afterwards. This is a memoryless waveshaper — it sees one sample, has no
/// idea what is coming, and recovers instantly.
///
/// On a gunshot or a laugh — the material this app exists for — the export
/// will pump where the preview does not, so the preview under-reports it. The
/// honest fix is a look-ahead envelope threaded through the mixer, which is a
/// real piece of DSP and is not what this is. Until then the comment says what
/// is true rather than what would be convenient: **matching ceiling,
/// different transient behaviour**.
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

    /// The dB the sliders speak in, against the linear gain the mixer and the
    /// `volume=` filter use. Checked against known values rather than against
    /// an inverse function that existed only for this test.
    #[test]
    fn known_gains() {
        for (db, linear) in [
            (0.0f32, 1.0f32),
            (-6.0, 0.5012),
            (6.0, 1.9953),
            (-20.0, 0.1),
        ] {
            let got = db_to_linear(db);
            assert!((got - linear).abs() < 1e-3, "{db} dB -> {got}");
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

    /// The promise the preview limiter actually makes, pinned so it stays the
    /// one being made: *the ceiling matches the export's, the dynamics do not*.
    ///
    /// The docs used to claim the preview "does not sound different from the
    /// export", which is true of steady tone and false of every transient —
    /// the export's `alimiter` has 5 ms of attack and 50 ms of release, and
    /// this has neither.
    #[test]
    fn the_limiter_matches_the_export_on_level_and_not_on_dynamics() {
        // Steady-state: a sustained overload settles at the same ceiling the
        // export is told to use, which is what makes the preview usable for
        // setting gain.
        for x in [1.0f32, 2.0, 8.0] {
            let out = limit_sample(x);
            assert!(out <= LIMITER_CEILING + 1e-6, "{x} -> {out}");
            assert!(out > LIMITER_CEILING - 0.1, "{x} -> {out}");
        }

        // Memoryless: the sample after a transient is untouched, where a
        // limiter with a 50 ms release would still be holding the level down.
        // This is the difference, stated as a test rather than as an
        // apology in a comment.
        assert_eq!(limit_sample(0.5), 0.5, "no release to recover from");
        assert_eq!(
            limit_sample(0.2),
            0.2,
            "a quiet sample right after a loud one is left alone"
        );
    }

    #[test]
    fn linear_arg_is_stable() {
        assert_eq!(linear_arg(0.0), "1.0000");
        assert_eq!(linear_arg(-6.0), "0.5012");
        assert_eq!(linear_arg(f32::NEG_INFINITY), "0.0000");
    }
}
