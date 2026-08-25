//! Microphone capture trait: open on press, closed on release, never held open.
//!
//! `CpalCapture` is the real backend: one `cpal::Stream` per recording, parked on its own
//! thread so it never crosses one, with the resample deferred to `stop()` over the whole
//! buffer. Produces an `Audio` at 16 kHz mono.

use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

use crate::audio::Audio;

pub trait Capture: Send {
    fn start(&mut self) -> Result<(), String>;
    fn stop(&mut self) -> Result<Audio, String>;
}

/// Stop signal plus join handle for the thread parked on one recording's stream.
type Worker = (Sender<()>, JoinHandle<Result<(), String>>);

pub struct CpalCapture {
    buffer: Arc<Mutex<Vec<f32>>>,
    channels: u16,
    rate: u32,
    worker: Option<Worker>,
}

impl CpalCapture {
    pub fn open_default() -> Result<CpalCapture, String> {
        let device = cpal::default_host()
            .default_input_device()
            .ok_or("no default input device")?;
        let cfg = device
            .default_input_config()
            .map_err(|e| format!("input config: {e}"))?;
        validate(cfg.sample_rate(), cfg.channels())?;
        Ok(CpalCapture {
            buffer: Arc::new(Mutex::new(Vec::new())),
            channels: cfg.channels(),
            rate: cfg.sample_rate(),
            worker: None,
        })
    }
}

/// `Audio::from_f32` reads a zero rate as "already at target, leave it alone" and clamps a
/// zero channel count to mono, so a device reporting either would yield silently-wrong audio
/// rather than an error. Reject it at the boundary instead.
fn validate(rate: u32, channels: u16) -> Result<(), String> {
    if rate == 0 {
        return Err(format!("device reports sample rate {rate}"));
    }
    if channels == 0 {
        return Err(format!("device reports {channels} channels"));
    }
    Ok(())
}

pub fn describe_default_device() -> Result<String, String> {
    let device = cpal::default_host()
        .default_input_device()
        .ok_or("no default input device")?;
    let cfg = device
        .default_input_config()
        .map_err(|e| format!("input config: {e}"))?;
    let name = device
        .description()
        .map(|d| d.name().to_string())
        .unwrap_or_else(|_| "unnamed".into());
    Ok(format!(
        "{} ({} Hz, {} ch, {:?})",
        name,
        cfg.sample_rate(),
        cfg.channels(),
        cfg.sample_format()
    ))
}

impl Capture for CpalCapture {
    fn start(&mut self) -> Result<(), String> {
        if self.worker.is_some() {
            return Err("already recording".into());
        }
        self.buffer.lock().unwrap().clear();
        let buffer = self.buffer.clone();
        let (stop_tx, stop_rx) = channel::<()>();
        let (ready_tx, ready_rx) = channel::<Result<(), String>>();
        let handle = std::thread::Builder::new()
            .name("byovox-capture".into())
            .spawn(move || {
                let built = build_stream(buffer);
                let stream = match built {
                    Ok(s) => s,
                    Err(e) => {
                        let _ = ready_tx.send(Err(e.clone()));
                        return Err(e);
                    }
                };
                if let Err(e) = stream.play() {
                    let _ = ready_tx.send(Err(e.to_string()));
                    return Err(e.to_string());
                }
                let _ = ready_tx.send(Ok(()));
                let _ = stop_rx.recv(); // blocks until stop() or the sender drops
                drop(stream);
                Ok(())
            })
            .map_err(|e| e.to_string())?;
        ready_rx
            .recv()
            .map_err(|_| "capture thread died".to_string())??;
        self.worker = Some((stop_tx, handle));
        Ok(())
    }

    fn stop(&mut self) -> Result<Audio, String> {
        let Some((stop_tx, handle)) = self.worker.take() else {
            return Err("not recording".into());
        };
        let _ = stop_tx.send(());
        handle
            .join()
            .map_err(|_| "capture thread panicked".to_string())??;
        let samples = std::mem::take(&mut *self.buffer.lock().unwrap());
        Ok(Audio::from_f32(&samples, self.channels, self.rate))
    }
}

fn build_stream(buffer: Arc<Mutex<Vec<f32>>>) -> Result<cpal::Stream, String> {
    let device = cpal::default_host()
        .default_input_device()
        .ok_or("no default input device")?;
    let cfg = device
        .default_input_config()
        .map_err(|e| format!("input config: {e}"))?;
    let err_fn = |e| tracing::error!(error = %e, "capture stream error");
    let stream = match cfg.sample_format() {
        cpal::SampleFormat::F32 => device.build_input_stream(
            &cfg.into(),
            move |data: &[f32], _| buffer.lock().unwrap().extend_from_slice(data),
            err_fn,
            None,
        ),
        cpal::SampleFormat::I16 => device.build_input_stream(
            &cfg.into(),
            move |data: &[i16], _| {
                buffer
                    .lock()
                    .unwrap()
                    .extend(data.iter().map(|s| *s as f32 / i16::MAX as f32))
            },
            err_fn,
            None,
        ),
        cpal::SampleFormat::U16 => device.build_input_stream(
            &cfg.into(),
            move |data: &[u16], _| {
                buffer
                    .lock()
                    .unwrap()
                    .extend(data.iter().map(|s| (*s as f32 - 32768.0) / 32768.0))
            },
            err_fn,
            None,
        ),
        other => return Err(format!("unsupported sample format {other:?}")),
    };
    stream.map_err(|e| format!("building input stream: {e}"))
}

#[cfg(test)]
mod tests {
    use super::validate;

    #[test]
    fn a_zero_rate_or_channel_count_is_rejected() {
        assert!(validate(48_000, 2).is_ok());
        assert!(
            validate(0, 2).unwrap_err().contains("sample rate 0"),
            "{:?}",
            validate(0, 2)
        );
        assert!(
            validate(48_000, 0).unwrap_err().contains("0 channels"),
            "{:?}",
            validate(48_000, 0)
        );
    }
}
