//! Microphone capture trait: open on press, closed on release, never held open.
//!
//! `CpalCapture` is the real backend: one `cpal::Stream` per recording, parked on its own
//! thread so it never crosses one, with the resample deferred to `stop()` over the whole
//! buffer. Produces an `Audio` at 16 kHz mono.

use std::sync::mpsc::{RecvTimeoutError, Sender, TryRecvError, channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

use crate::audio::Audio;

pub trait Capture: Send {
    fn start(&mut self) -> Result<(), String>;
    fn stop(&mut self) -> Result<Audio, String>;
}

/// How long `start()` waits for the device to actually begin streaming.
const READY_TIMEOUT: Duration = Duration::from_secs(5);

/// Callback headroom: enough for this many seconds at the device's layout, so the audio
/// callback never reallocates mid-recording.
const RESERVE_SECS: usize = 30;

/// One recording's entire mutable state: the thread, its stop signal, and the two slots its
/// stream writes into. Every field is allocated fresh by `start()` and dropped by `stop()`, so
/// a thread abandoned by a timed-out `start()` shares nothing with the recording that follows
/// it — its writes land in Arcs nobody will ever read again.
struct Recording {
    stop_tx: Sender<()>,
    handle: JoinHandle<Result<(), String>>,
    buffer: Arc<Mutex<Vec<f32>>>,
    /// First error the stream reported, if any; a recording that hit one is failed, not
    /// silently returned truncated.
    error: Arc<Mutex<Option<String>>>,
}

pub struct CpalCapture {
    /// The layout the *running* stream opened with. Seeded from the default config at
    /// construction and overwritten by `start()` once the stream is up, because WASAPI
    /// re-reads `GetMixFormat` on every query and a shared-mode format change would
    /// otherwise leave `stop()` resampling against a stale rate.
    channels: u16,
    rate: u32,
    worker: Option<Recording>,
}

impl CpalCapture {
    pub fn open_default() -> Result<CpalCapture, String> {
        let (_device, cfg) = default_input()?;
        Ok(CpalCapture {
            channels: cfg.channels(),
            rate: cfg.sample_rate(),
            worker: None,
        })
    }
}

/// The default input device and its current default config, rejected here if we could not
/// record it faithfully — so `check` reports an unrecordable device before the first
/// push-to-talk rather than at the first dictation.
fn default_input() -> Result<(cpal::Device, cpal::SupportedStreamConfig), String> {
    let device = cpal::default_host()
        .default_input_device()
        .ok_or("no default input device")?;
    let cfg = device
        .default_input_config()
        .map_err(|e| format!("input config: {e}"))?;
    validate(cfg.sample_rate(), cfg.channels(), cfg.sample_format())?;
    Ok((device, cfg))
}

/// `Audio::from_f32` reads a zero rate as "already at target, leave it alone" and clamps a
/// zero channel count to mono, so a device reporting either would yield silently-wrong audio
/// rather than an error. Reject it at the boundary instead.
fn validate_layout(rate: u32, channels: u16) -> Result<(), String> {
    if rate == 0 {
        return Err(format!("device reports sample rate {rate}"));
    }
    if channels == 0 {
        return Err(format!("device reports {channels} channels"));
    }
    Ok(())
}

/// `validate_layout` plus a sample format `build_stream` knows how to decode.
fn validate(rate: u32, channels: u16, format: cpal::SampleFormat) -> Result<(), String> {
    validate_layout(rate, channels)?;
    if !matches!(
        format,
        cpal::SampleFormat::F32 | cpal::SampleFormat::I16 | cpal::SampleFormat::U16
    ) {
        return Err(format!("unsupported sample format {format:?}"));
    }
    Ok(())
}

pub fn describe_default_device() -> Result<String, String> {
    let (device, cfg) = default_input()?;
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
        // Fresh per recording, never reused: a straggler thread from an abandoned start() keeps
        // writing into its own copies, where nothing can read them again.
        let buffer = Arc::new(Mutex::new(Vec::with_capacity(
            RESERVE_SECS * self.rate as usize * self.channels.max(1) as usize,
        )));
        let error = Arc::new(Mutex::new(None));
        let (thread_buffer, thread_error) = (buffer.clone(), error.clone());
        let (stop_tx, stop_rx) = channel::<()>();
        let (ready_tx, ready_rx) = channel::<Result<(u32, u16), String>>();
        let handle = std::thread::Builder::new()
            .name("byovox-capture".into())
            .spawn(move || {
                let built = build_stream(thread_buffer, thread_error);
                let (stream, rate, channels) = match built {
                    Ok(s) => s,
                    Err(e) => {
                        let _ = ready_tx.send(Err(e.clone()));
                        return Err(e);
                    }
                };
                // WASAPI init can outlast start()'s deadline. If it did, start() has already
                // dropped stop_tx and moved on, so release the device without ever playing it:
                // an abandoned recording must not put a live microphone into the world.
                if !should_play(stop_rx.try_recv()) {
                    drop(stream);
                    return Ok(());
                }
                if let Err(e) = stream.play() {
                    let _ = ready_tx.send(Err(e.to_string()));
                    return Err(e.to_string());
                }
                let _ = ready_tx.send(Ok((rate, channels)));
                let _ = stop_rx.recv(); // blocks until stop(), or until the sender drops
                drop(stream);
                Ok(())
            })
            .map_err(|e| e.to_string())?;
        let (rate, channels) = match ready_rx.recv_timeout(READY_TIMEOUT) {
            Ok(ready) => ready?,
            // Abandon the recording. Dropping stop_tx closes stop_rx, which the thread checks
            // before playing: a stream still being built inside WASAPI is therefore dropped
            // unplayed, and one that somehow got past that check stops at its stop_rx.recv().
            // Either way the straggler only ever touches this recording's own buffer and error
            // slot, which die with it — the next start() allocates its own.
            Err(RecvTimeoutError::Timeout) => {
                drop(stop_tx);
                return Err("microphone did not start within 5 s".into());
            }
            Err(RecvTimeoutError::Disconnected) => return Err("capture thread died".into()),
        };
        // The stream may have opened at a different layout than open_default() saw. Abandoning
        // here is safe for the same reason: the stream is playing, but dropping stop_tx ends it,
        // and its slots are this recording's.
        if let Err(e) = validate_layout(rate, channels) {
            drop(stop_tx);
            return Err(e);
        }
        self.rate = rate;
        self.channels = channels;
        self.worker = Some(Recording {
            stop_tx,
            handle,
            buffer,
            error,
        });
        Ok(())
    }

    fn stop(&mut self) -> Result<Audio, String> {
        let Some(rec) = self.worker.take() else {
            return Err("not recording".into());
        };
        let _ = rec.stop_tx.send(());
        rec.handle
            .join()
            .map_err(|_| "capture thread panicked".to_string())??;
        let samples = std::mem::take(&mut *rec.buffer.lock().unwrap());
        if let Some(e) = rec.error.lock().unwrap().take() {
            return Err(format!("capture stream error: {e}"));
        }
        Ok(Audio::from_f32(&samples, self.channels, self.rate))
    }
}

/// Whether the capture thread should play the stream it has just built. Only an empty channel
/// means `start()` is still waiting for us: `Disconnected` is a `start()` that timed out and
/// abandoned this recording, and a delivered `Ok(())` is a stop that beat the stream up. In
/// both cases the stream is dropped unplayed, so the microphone never goes live behind a caller
/// that has already given up on it.
fn should_play(signal: Result<(), TryRecvError>) -> bool {
    matches!(signal, Err(TryRecvError::Empty))
}

/// Returns the stream plus the layout it actually opened with.
fn build_stream(
    buffer: Arc<Mutex<Vec<f32>>>,
    error: Arc<Mutex<Option<String>>>,
) -> Result<(cpal::Stream, u32, u16), String> {
    let (device, cfg) = default_input()?;
    let (rate, channels) = (cfg.sample_rate(), cfg.channels());
    let err_fn = move |e: cpal::StreamError| {
        tracing::error!(error = %e, "capture stream error");
        let mut slot = error.lock().unwrap();
        if slot.is_none() {
            *slot = Some(e.to_string());
        }
    };
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
    stream
        .map(|s| (s, rate, channels))
        .map_err(|e| format!("building input stream: {e}"))
}

#[cfg(test)]
mod tests {
    use super::{Capture, CpalCapture, TryRecvError, should_play, validate, validate_layout};
    use cpal::SampleFormat;

    #[test]
    fn a_zero_rate_or_channel_count_is_rejected() {
        assert!(validate_layout(48_000, 2).is_ok());
        assert!(
            validate_layout(0, 2).unwrap_err().contains("sample rate 0"),
            "{:?}",
            validate_layout(0, 2)
        );
        assert!(
            validate_layout(48_000, 0)
                .unwrap_err()
                .contains("0 channels"),
            "{:?}",
            validate_layout(48_000, 0)
        );
    }

    #[test]
    fn only_the_decodable_sample_formats_are_accepted() {
        for f in [SampleFormat::F32, SampleFormat::I16, SampleFormat::U16] {
            assert!(validate(48_000, 2, f).is_ok(), "{f:?}");
        }
        for f in [
            SampleFormat::I8,
            SampleFormat::I24,
            SampleFormat::I32,
            SampleFormat::U8,
            SampleFormat::U32,
            SampleFormat::F64,
        ] {
            let e = validate(48_000, 2, f).unwrap_err();
            assert!(e.contains("unsupported sample format"), "{f:?}: {e}");
            assert!(e.contains(&format!("{f:?}")), "{f:?}: {e}");
        }
    }

    #[test]
    fn a_bad_layout_is_reported_before_the_format() {
        let e = validate(0, 2, SampleFormat::F64).unwrap_err();
        assert!(e.contains("sample rate 0"), "{e}");
    }

    #[test]
    fn a_built_stream_is_played_only_while_start_is_still_waiting() {
        assert!(should_play(Err(TryRecvError::Empty)), "start still waiting");
        assert!(
            !should_play(Err(TryRecvError::Disconnected)),
            "start timed out and abandoned us"
        );
        assert!(!should_play(Ok(())), "a stop beat the stream up");
    }

    /// All per-recording state lives in `worker`, so a `CpalCapture` that has never started
    /// owns no buffer and no error slot to confuse — and needs no device to say so.
    #[test]
    fn stop_before_any_start_is_an_error() {
        let mut cap = CpalCapture {
            channels: 2,
            rate: 48_000,
            worker: None,
        };
        assert_eq!(cap.stop().unwrap_err(), "not recording");
    }
}
