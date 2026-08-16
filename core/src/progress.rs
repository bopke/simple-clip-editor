//! Parser for `-progress pipe:1 -nostats`. Never scrape stderr.

use crate::time::RationalTime;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ProgressUpdate {
    pub frame: Option<u64>,
    pub out_time: Option<RationalTime>,
    pub speed: Option<f32>,
    /// ffmpeg emits `progress=end` on the final block.
    pub done: bool,
}

/// Feed lines as they arrive; returns an update once a block completes.
#[derive(Default)]
pub struct ProgressParser {
    current: ProgressUpdate,
}

impl ProgressParser {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn feed_line(&mut self, line: &str) -> Option<ProgressUpdate> {
        let (key, value) = line.trim().split_once('=')?;
        let value = value.trim();
        match key.trim() {
            "frame" => self.current.frame = value.parse().ok(),
            "out_time_us" | "out_time_ms" => {
                // Despite the name, ffmpeg emits microseconds for both keys.
                if let Ok(us) = value.parse::<i64>() {
                    self.current.out_time = Some(RationalTime::new(us, 1_000_000));
                }
            }
            "speed" => {
                self.current.speed = value.trim_end_matches('x').trim().parse().ok();
            }
            "progress" => {
                self.current.done = value == "end";
                let out = self.current;
                self.current = ProgressUpdate::default();
                return Some(out);
            }
            _ => {}
        }
        None
    }
}

/// Fraction of `total` covered by an update, clamped to 0..=1.
pub fn fraction(update: &ProgressUpdate, total: RationalTime) -> f32 {
    if total.num <= 0 {
        return 0.0;
    }
    match update.out_time {
        Some(t) => (t.as_f64() / total.as_f64()).clamp(0.0, 1.0) as f32,
        None => 0.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_progress_block() {
        let mut p = ProgressParser::new();
        for line in [
            "frame=120",
            "fps=58.3",
            "bitrate=2000kbits/s",
            "out_time_us=2000000",
            "speed=1.5x",
        ] {
            assert!(p.feed_line(line).is_none());
        }
        let u = p.feed_line("progress=continue").unwrap();
        assert_eq!(u.frame, Some(120));
        assert_eq!(u.out_time.unwrap().to_ffmpeg_arg(), "2.0");
        assert_eq!(u.speed, Some(1.5));
        assert!(!u.done);
    }

    #[test]
    fn detects_end() {
        let mut p = ProgressParser::new();
        p.feed_line("out_time_us=5000000");
        let u = p.feed_line("progress=end").unwrap();
        assert!(u.done);
        assert_eq!(fraction(&u, RationalTime::new(10, 1)), 0.5);
    }

    #[test]
    fn ignores_noise() {
        let mut p = ProgressParser::new();
        assert!(p.feed_line("").is_none());
        assert!(p.feed_line("garbage").is_none());
        assert!(p.feed_line("out_time_us=N/A").is_none());
    }
}
