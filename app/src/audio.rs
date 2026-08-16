//! Decoded PCM and the waveform peaks derived from it.
//!
//! For trimming, the waveform is often better than audio for finding cut
//! points — the gunshot, the laugh, the silence are all visible. Combined with
//! frame-stepping this makes the app usable before playback exists.

use anyhow::{Context, Result};
use clip_core::command::{PCM_CHANNELS, PCM_SAMPLE_RATE};
use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::Path;
use std::sync::Arc;

/// Audio frames per waveform bucket. ~94 buckets/second at 48 kHz, which is
/// more resolution than any sane timeline zoom needs.
pub const BUCKET_FRAMES: usize = 512;

/// Refuse to load absurd amounts of PCM into memory. 90 minutes of stereo f32
/// at 48 kHz is about 2 GB; past that, playback is disabled rather than the
/// process being killed by the OS.
const MAX_PCM_BYTES: u64 = 2_000_000_000;

#[derive(Clone, Debug, Default)]
pub struct Waveform {
    pub min: Vec<f32>,
    pub max: Vec<f32>,
}

impl Waveform {
    pub fn is_empty(&self) -> bool {
        self.max.is_empty()
    }

    pub fn buckets(&self) -> usize {
        self.max.len()
    }

    /// Peak envelope over a bucket range, for one column of the drawing.
    pub fn range(&self, from: usize, to: usize) -> (f32, f32) {
        let to = to.min(self.max.len());
        if from >= to {
            return (0.0, 0.0);
        }
        let lo = self.min[from..to].iter().copied().fold(0.0f32, f32::min);
        let hi = self.max[from..to].iter().copied().fold(0.0f32, f32::max);
        (lo, hi)
    }

    /// Bucket index for a time in seconds.
    pub fn bucket_at(&self, seconds: f64) -> usize {
        ((seconds * PCM_SAMPLE_RATE as f64) / BUCKET_FRAMES as f64) as usize
    }

    pub fn compute(pcm_path: &Path) -> Result<Waveform> {
        let file =
            File::open(pcm_path).with_context(|| format!("opening {}", pcm_path.display()))?;
        let mut reader = BufReader::with_capacity(1 << 20, file);
        let mut wf = Waveform::default();
        let frame_bytes = 4 * PCM_CHANNELS as usize;
        let mut buf = vec![0u8; BUCKET_FRAMES * frame_bytes];
        loop {
            let n = read_full(&mut reader, &mut buf)?;
            if n == 0 {
                break;
            }
            let (mut lo, mut hi) = (0.0f32, 0.0f32);
            for chunk in buf[..n].chunks_exact(4) {
                let v = f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
                if v.is_finite() {
                    lo = lo.min(v);
                    hi = hi.max(v);
                }
            }
            wf.min.push(lo);
            wf.max.push(hi);
            if n < buf.len() {
                break;
            }
        }
        Ok(wf)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let mut w = BufWriter::new(File::create(path)?);
        w.write_all(&(self.max.len() as u64).to_le_bytes())?;
        for (lo, hi) in self.min.iter().zip(&self.max) {
            w.write_all(&lo.to_le_bytes())?;
            w.write_all(&hi.to_le_bytes())?;
        }
        w.flush()?;
        Ok(())
    }

    pub fn load(path: &Path) -> Result<Waveform> {
        let mut r = BufReader::new(File::open(path)?);
        let mut len = [0u8; 8];
        r.read_exact(&mut len)?;
        let len = u64::from_le_bytes(len) as usize;
        let mut wf = Waveform {
            min: Vec::with_capacity(len),
            max: Vec::with_capacity(len),
        };
        let mut pair = [0u8; 8];
        for _ in 0..len {
            r.read_exact(&mut pair)?;
            wf.min
                .push(f32::from_le_bytes([pair[0], pair[1], pair[2], pair[3]]));
            wf.max
                .push(f32::from_le_bytes([pair[4], pair[5], pair[6], pair[7]]));
        }
        Ok(wf)
    }
}

fn read_full(r: &mut impl Read, buf: &mut [u8]) -> Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        let n = r.read(&mut buf[filled..])?;
        if n == 0 {
            break;
        }
        filled += n;
    }
    Ok(filled)
}

/// Interleaved stereo f32 at 48 kHz — the format the mixer works in, so the
/// audio callback never has to convert anything.
#[derive(Debug, Default)]
pub struct Pcm {
    pub samples: Vec<f32>,
}

impl Pcm {
    pub fn load(path: &Path) -> Result<Arc<Pcm>> {
        let meta = std::fs::metadata(path)?;
        if meta.len() > MAX_PCM_BYTES {
            anyhow::bail!("source audio is too long to hold in memory for playback");
        }
        let mut file = File::open(path)?;
        let mut bytes = Vec::with_capacity(meta.len() as usize);
        file.read_to_end(&mut bytes)?;
        let samples = bytes
            .chunks_exact(4)
            .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect();
        Ok(Arc::new(Pcm { samples }))
    }

    pub fn frames(&self) -> usize {
        self.samples.len() / PCM_CHANNELS as usize
    }

    pub fn duration_secs(&self) -> f64 {
        self.frames() as f64 / PCM_SAMPLE_RATE as f64
    }

    /// One stereo frame. Out-of-range reads are silence rather than a panic —
    /// the mixer runs in the audio callback and must never fail.
    #[inline]
    pub fn frame(&self, i: isize) -> (f32, f32) {
        if i < 0 {
            return (0.0, 0.0);
        }
        let i = i as usize * PCM_CHANNELS as usize;
        match (self.samples.get(i), self.samples.get(i + 1)) {
            (Some(l), Some(r)) => (*l, *r),
            _ => (0.0, 0.0),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_pcm(path: &Path, samples: &[f32]) {
        let mut bytes = Vec::new();
        for s in samples {
            bytes.extend_from_slice(&s.to_le_bytes());
        }
        std::fs::write(path, bytes).unwrap();
    }

    #[test]
    fn peaks_capture_min_and_max() {
        let path = std::env::temp_dir().join("clip-editor-wf-test.f32");
        let mut samples = vec![0.0f32; BUCKET_FRAMES * 2 * 2];
        samples[10] = 0.75;
        samples[11] = -0.5;
        // Second bucket stays silent.
        write_pcm(&path, &samples);
        let wf = Waveform::compute(&path).unwrap();
        assert_eq!(wf.buckets(), 2);
        assert!((wf.max[0] - 0.75).abs() < 1e-6);
        assert!((wf.min[0] + 0.5).abs() < 1e-6);
        assert_eq!(wf.range(1, 2), (0.0, 0.0));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn waveform_roundtrips_through_disk() {
        let path = std::env::temp_dir().join("clip-editor-wf-test.bin");
        let wf = Waveform {
            min: vec![-0.5, -0.25],
            max: vec![0.5, 0.25],
        };
        wf.save(&path).unwrap();
        let back = Waveform::load(&path).unwrap();
        assert_eq!(back.min, wf.min);
        assert_eq!(back.max, wf.max);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn pcm_reads_out_of_range_as_silence() {
        let pcm = Pcm {
            samples: vec![0.1, 0.2],
        };
        assert_eq!(pcm.frames(), 1);
        assert_eq!(pcm.frame(0), (0.1, 0.2));
        assert_eq!(pcm.frame(5), (0.0, 0.0));
        assert_eq!(pcm.frame(-1), (0.0, 0.0));
    }
}
