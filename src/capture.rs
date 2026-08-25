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
    /// The `capture.device` selector this capture was opened with, so every recording's stream
    /// reopens the microphone the config chose rather than whatever the system default has
    /// become since the daemon started.
    device: String,
    /// The layout the *running* stream opened with. Seeded from the default config at
    /// construction and overwritten by `start()` once the stream is up, because WASAPI
    /// re-reads `GetMixFormat` on every query and a shared-mode format change would
    /// otherwise leave `stop()` resampling against a stale rate.
    channels: u16,
    rate: u32,
    worker: Option<Recording>,
}

impl CpalCapture {
    /// `selector` is `capture.device`: empty for the system default input device, otherwise a
    /// case-insensitive substring of the name of the one to record from.
    pub fn open(selector: &str) -> Result<CpalCapture, String> {
        let (_device, cfg) = input_device(selector)?;
        Ok(CpalCapture {
            device: selector.to_string(),
            channels: cfg.channels(),
            rate: cfg.sample_rate(),
            worker: None,
        })
    }
}

/// The input device `capture.device` selects and its current default config, rejected here if
/// we could not record it faithfully — so `check` reports an unrecordable device before the
/// first push-to-talk rather than at the first dictation.
fn input_device(selector: &str) -> Result<(cpal::Device, cpal::SupportedStreamConfig), String> {
    let device = match selector.trim() {
        // Nothing is enumerated for the default: the selector exists to keep byovox off a
        // device, so leaving it empty must reach WASAPI exactly as it did before it existed.
        "" => cpal::default_host()
            .default_input_device()
            .ok_or("no default input device")?,
        wanted => {
            let mut devices = input_devices()?;
            let chosen = resolve(wanted, &names(&devices))?;
            devices.swap_remove(chosen)
        }
    };
    let cfg = device
        .default_input_config()
        .map_err(|e| format!("input config: {e}"))?;
    // Named before it is judged. Without this a device rejected for its layout or sample
    // format reaches `check` as a bare "unsupported sample format I24", with the handle that
    // could have said which device one line above it.
    validate(cfg.sample_rate(), cfg.channels(), cfg.sample_format())
        .map_err(|e| format!("{}: {e}", device_name(&device)))?;
    Ok((device, cfg))
}

/// Every input device the host offers, in enumeration order — the order `select` resolves a
/// selector in and the order `check` lists them for the user to copy a name out of.
fn input_devices() -> Result<Vec<cpal::Device>, String> {
    cpal::default_host()
        .input_devices()
        .map(|d| d.collect())
        .map_err(|e| format!("listing input devices: {e}"))
}

fn names(devices: &[cpal::Device]) -> Vec<String> {
    devices.iter().map(device_name).collect()
}

/// The names of every input device, for the list `check` prints and the one a bad
/// `capture.device` is refused with.
pub fn input_names() -> Result<Vec<String>, String> {
    Ok(names(&input_devices()?))
}

/// Which device a non-empty `capture.device` names: the first whose name contains it, compared
/// case-insensitively. A substring rather than the whole name because the name a host reports
/// is the driver's, not the one on the Sound control panel, and a word out of it — `Array` —
/// is what the user can be sure of.
///
/// The error names the key and the value; `resolve` adds the list of what it could have
/// matched. Pure, so the rule is tested without a device.
fn select(selector: &str, names: &[String]) -> Result<usize, String> {
    let wanted = selector.trim().to_lowercase();
    // Every substring contains the empty one, so an unguarded empty selector would match the
    // first device enumerated and call it a choice. The default device is not chosen from a
    // list; both callers keep it out of here, and this is the refusal if one ever stops.
    if wanted.is_empty() {
        return Err("capture.device is empty: that is the system default, not a name".into());
    }
    names
        .iter()
        .position(|n| n.to_lowercase().contains(&wanted))
        .ok_or_else(|| {
            format!(
                "capture.device {:?} matched no input device",
                selector.trim()
            )
        })
}

/// `select` with the names it could have matched appended: the only form of the refusal a user
/// ever sees, so a bad `capture.device` says what the good ones are wherever it surfaces — the
/// daemon's startup error, or a `check` row.
fn resolve(selector: &str, names: &[String]) -> Result<usize, String> {
    select(selector, names).map_err(|e| format!("{e}; available: {}", names.join(" | ")))
}

/// Fails when `capture.device` names no input device, naming the key and listing every name it
/// could have matched. The daemon runs this at startup — before it installs a hook or opens
/// anything — because a selector nothing matches would otherwise surface as a failed dictation
/// long after the console that could have reported it is gone.
pub fn validate_selector(selector: &str) -> Result<(), String> {
    if selector.trim().is_empty() {
        return Ok(());
    }
    resolve(selector, &input_names()?).map(|_| ())
}

/// The device's own name, or `unnamed` — never a reason to fail.
fn device_name(device: &cpal::Device) -> String {
    device
        .description()
        .map(|d| d.name().to_string())
        .unwrap_or_else(|_| "unnamed".into())
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

/// What a chosen microphone reports about itself: `check` prints it and decides from the same
/// two fields whether it is about to record through a Bluetooth hands-free profile.
#[derive(Debug, Clone, PartialEq)]
pub struct DeviceInfo {
    pub name: String,
    pub rate: u32,
    pub channels: u16,
    pub format: cpal::SampleFormat,
}

impl std::fmt::Display for DeviceInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} ({} Hz, {} ch, {:?})",
            self.name, self.rate, self.channels, self.format
        )
    }
}

pub fn describe_device(selector: &str) -> Result<DeviceInfo, String> {
    let (device, cfg) = input_device(selector)?;
    Ok(DeviceInfo {
        name: device_name(&device),
        rate: cfg.sample_rate(),
        channels: cfg.channels(),
        format: cfg.sample_format(),
    })
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
        let selector = self.device.clone();
        let handle = std::thread::Builder::new()
            .name("byovox-capture".into())
            .spawn(move || {
                let built = build_stream(&selector, thread_buffer, thread_error);
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
        // The stream may have opened at a different layout than open() saw. Abandoning
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
    selector: &str,
    buffer: Arc<Mutex<Vec<f32>>>,
    error: Arc<Mutex<Option<String>>>,
) -> Result<(cpal::Stream, u32, u16), String> {
    let (device, cfg) = input_device(selector)?;
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
    use super::{
        Capture, CpalCapture, DeviceInfo, TryRecvError, resolve, select, should_play, validate,
        validate_layout,
    };
    use cpal::SampleFormat;

    /// Names in the shape a host reports them — the driver's, not the Sound panel's.
    fn devices() -> Vec<String> {
        [
            "Microphone Array (Synaptics Audio)",
            "Headset (PaMu Slide Hands-Free)",
        ]
        .map(String::from)
        .to_vec()
    }

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
            device: String::new(),
            channels: 2,
            rate: 48_000,
            worker: None,
        };
        assert_eq!(cap.stop().unwrap_err(), "not recording");
    }

    /// The selector is a substring, matched without regard to case: the name a host reports is
    /// the driver's, and the user types the part of it they recognise.
    #[test]
    fn a_selector_matches_any_part_of_a_device_name_in_any_case() {
        let d = devices();
        assert_eq!(select("Microphone Array", &d), Ok(0));
        assert_eq!(select("microphone array", &d), Ok(0));
        assert_eq!(select("SYNAPTICS", &d), Ok(0));
        assert_eq!(select("Hands-Free", &d), Ok(1));
        // Padding in the file is not part of the name the user meant.
        assert_eq!(select("  Headset  ", &d), Ok(1));
    }

    /// Two microphones can share a word — an external one and the built-in array both answer
    /// to "Microphone". Enumeration order decides, and `check` prints which one that made it.
    #[test]
    fn the_first_device_in_enumeration_order_wins() {
        let d = vec![
            "Microphone (USB Audio)".to_string(),
            "Microphone Array (Synaptics Audio)".to_string(),
        ];
        assert_eq!(select("Microphone", &d), Ok(0));
        assert_eq!(select("Microphone Array", &d), Ok(1));
    }

    /// A name nothing matches is a typo or a device that is not plugged in, and either way the
    /// refusal has to name the key, the value, and — once `resolve` has added them — every name
    /// it could have been instead, since that is the whole list the user gets to correct it
    /// from when the daemon refuses to start.
    #[test]
    fn a_selector_that_matches_nothing_names_the_key_the_value_and_the_alternatives() {
        let e = select("nonexistent", &devices()).unwrap_err();
        assert_eq!(e, "capture.device \"nonexistent\" matched no input device");
        assert_eq!(
            resolve("nonexistent", &devices()).unwrap_err(),
            "capture.device \"nonexistent\" matched no input device; available: \
             Microphone Array (Synaptics Audio) | Headset (PaMu Slide Hands-Free)"
        );
        assert_eq!(resolve("Headset", &devices()), Ok(1));
        // Not a wildcard: an empty selector is the system default, and no caller resolves it
        // through a list. Reaching here with one must not silently pick the first device.
        assert!(select("", &devices()).is_err());
        assert!(select("   ", &devices()).is_err());
        assert!(select("Microphone Array", &[]).is_err());
    }

    /// `check`'s `mic` row is what a bug report pastes, so the device's own layout has to be
    /// in it: the issue this selector exists for was diagnosed from `16000 Hz, 1 ch`.
    #[test]
    fn a_device_describes_itself_with_the_layout_it_reports() {
        let info = DeviceInfo {
            name: "Headset (PaMu Slide Hands-Free)".into(),
            rate: 16_000,
            channels: 1,
            format: SampleFormat::F32,
        };
        assert_eq!(
            info.to_string(),
            "Headset (PaMu Slide Hands-Free) (16000 Hz, 1 ch, F32)"
        );
    }
}
