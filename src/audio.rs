//! Audio buffer: whatever the device gives → 16 kHz mono i16, plus WAV encoding.
//!
//! Consumed by the pipeline (`Audio::to_wav` feeds the STT request) and by `check`
//! (`peak_dbfs` catches a muted or attenuated microphone before any request is made).

use std::io::Cursor;

pub const SAMPLE_RATE: u32 = 16_000;

#[derive(Clone, Debug, PartialEq)]
pub struct Audio {
    /// 16 kHz mono, 16-bit.
    pub samples: Vec<i16>,
}

impl Audio {
    /// Interleaved f32 in the device's own rate/channels → 16 kHz mono i16.
    pub fn from_f32(input: &[f32], channels: u16, rate: u32) -> Audio {
        let mono = downmix(input, channels.max(1) as usize);
        let at_16k = resample(&mono, rate, SAMPLE_RATE);
        let samples = at_16k
            .iter()
            .map(|v| (v.clamp(-1.0, 1.0) * i16::MAX as f32).round() as i16)
            .collect();
        Audio { samples }
    }

    pub fn duration_secs(&self) -> f32 {
        self.samples.len() as f32 / SAMPLE_RATE as f32
    }

    /// Peak level in dBFS; -120 for digital silence.
    pub fn peak_dbfs(&self) -> f32 {
        let peak = self
            .samples
            .iter()
            .map(|s| (*s as i32).abs())
            .max()
            .unwrap_or(0);
        if peak == 0 {
            return -120.0;
        }
        20.0 * (peak as f32 / i16::MAX as f32).log10()
    }

    pub fn to_wav(&self) -> Vec<u8> {
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: SAMPLE_RATE,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut cursor = Cursor::new(Vec::new());
        {
            let mut w = hound::WavWriter::new(&mut cursor, spec).expect("in-memory writer");
            for s in &self.samples {
                w.write_sample(*s).expect("in-memory write");
            }
            w.finalize().expect("in-memory finalize");
        }
        cursor.into_inner()
    }
}

fn downmix(input: &[f32], channels: usize) -> Vec<f32> {
    if channels == 1 {
        return input.to_vec();
    }
    input
        .chunks_exact(channels)
        .map(|frame| frame.iter().sum::<f32>() / channels as f32)
        .collect()
}

// ponytail: box prefilter + linear interpolation. Whisper is tolerant of this; swap in
// rubato (windowed sinc) if accuracy on 44.1 kHz microphones ever disappoints.
fn resample(mono: &[f32], from: u32, to: u32) -> Vec<f32> {
    if from == to || mono.is_empty() {
        return mono.to_vec();
    }
    let filtered: Vec<f32> = if from > to {
        // Average over the decimation window to damp aliasing before interpolating.
        let win = (from as f32 / to as f32).round().max(1.0) as usize;
        (0..mono.len())
            .map(|i| {
                let lo = i.saturating_sub(win / 2);
                let hi = (i + win / 2 + 1).min(mono.len());
                mono[lo..hi].iter().sum::<f32>() / (hi - lo) as f32
            })
            .collect()
    } else {
        mono.to_vec()
    };
    let out_len = ((mono.len() as u64 * to as u64) / from as u64) as usize;
    let step = from as f64 / to as f64;
    (0..out_len)
        .map(|i| {
            let pos = i as f64 * step;
            let idx = pos.floor() as usize;
            let frac = (pos - idx as f64) as f32;
            let a = filtered[idx.min(filtered.len() - 1)];
            let b = filtered[(idx + 1).min(filtered.len() - 1)];
            a + (b - a) * frac
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(rate: u32, channels: u16, secs: f32, hz: f32, amp: f32) -> Vec<f32> {
        let n = (rate as f32 * secs) as usize;
        (0..n)
            .flat_map(|i| {
                let v = amp * (2.0 * std::f32::consts::PI * hz * i as f32 / rate as f32).sin();
                std::iter::repeat_n(v, channels as usize)
            })
            .collect()
    }

    #[test]
    fn stereo_48k_becomes_mono_16k_of_the_same_duration() {
        let a = Audio::from_f32(&sine(48_000, 2, 1.0, 440.0, 0.5), 2, 48_000);
        assert_eq!(a.samples.len(), 16_000);
        assert!((a.duration_secs() - 1.0).abs() < 0.001);
    }

    #[test]
    fn already_16k_mono_is_untouched_in_length() {
        let a = Audio::from_f32(&sine(16_000, 1, 0.5, 440.0, 0.5), 1, 16_000);
        assert_eq!(a.samples.len(), 8_000);
    }

    #[test]
    fn peak_is_reported_in_dbfs() {
        let half = Audio::from_f32(&sine(16_000, 1, 0.1, 440.0, 0.5), 1, 16_000);
        assert!(
            (half.peak_dbfs() - (-6.0)).abs() < 0.3,
            "{}",
            half.peak_dbfs()
        );
        let silence = Audio {
            samples: vec![0; 1600],
        };
        assert!(silence.peak_dbfs() <= -120.0);
    }

    #[test]
    fn wav_is_16k_mono_16bit_pcm() {
        let a = Audio::from_f32(&sine(16_000, 1, 0.1, 440.0, 0.5), 1, 16_000);
        let bytes = a.to_wav();
        let reader = hound::WavReader::new(std::io::Cursor::new(&bytes)).unwrap();
        let spec = reader.spec();
        assert_eq!(
            (spec.sample_rate, spec.channels, spec.bits_per_sample),
            (16_000, 1, 16)
        );
        assert_eq!(reader.len() as usize, a.samples.len());
        assert_eq!(&bytes[..4], b"RIFF");
    }
}
