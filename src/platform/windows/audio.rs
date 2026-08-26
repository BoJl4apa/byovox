//! Watching the default audio *render* endpoint.
//!
//! Depends on the WASAPI device enumerator (`IMMDeviceEnumerator`) and on the COM apartment of
//! the thread that builds it. Produces one callback per Windows notification that could have
//! invalidated a stream on the default render endpoint: the default moving, an endpoint being
//! removed or changing state, and a shared-mode format change. It decides nothing and holds no
//! audio stream: what to do about a change is the caller's — `ui::App` re-opens its cue sink,
//! which is the whole of issue #7.

use windows::Win32::Foundation::{PROPERTYKEY, RPC_E_CHANGED_MODE};
use windows::Win32::Media::Audio::{
    DEVICE_STATE, EDataFlow, ERole, IMMDeviceEnumerator, IMMNotificationClient,
    IMMNotificationClient_Impl, MMDeviceEnumerator, PKEY_AudioEngine_DeviceFormat, eConsole,
    eMultimedia, eRender,
};
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize,
};
use windows::core::{PCWSTR, Result as ComResult, implement};

/// A live registration for default-render-endpoint changes. Unregisters on drop.
///
/// Field order is the teardown order and is load-bearing: `Drop` unregisters first, then the
/// fields are released in declaration order, and the apartment guard is last so
/// `CoUninitialize` runs after both COM pointers have been released.
pub struct DefaultRenderWatch {
    enumerator: IMMDeviceEnumerator,
    client: IMMNotificationClient,
    _com: Apartment,
}

impl DefaultRenderWatch {
    /// `on_change` runs on a WASAPI worker thread, not the caller's. Windows documents that a
    /// notification callback must not block and must not call back into the audio APIs, so the
    /// intended shape is to post to a channel and return — which is why the bound is
    /// `Send + Sync` rather than the caller's thread.
    pub fn new(on_change: impl Fn() + Send + Sync + 'static) -> Result<DefaultRenderWatch, String> {
        let com = Apartment::enter()?;
        // SAFETY: a documented CLSID, no aggregation, and this thread is in an apartment for
        // the whole call. The enumerator is `ThreadingModel=Both`, so it is created in place.
        let enumerator: IMMDeviceEnumerator =
            unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }
                .map_err(|e| format!("audio device enumerator: {e}"))?;
        let client: IMMNotificationClient = Notify {
            on_change: Box::new(on_change),
        }
        .into();
        // SAFETY: `client` is a live interface pointer this struct then owns until `Drop`
        // unregisters it, so Windows never calls into a released object.
        unsafe { enumerator.RegisterEndpointNotificationCallback(&client) }
            .map_err(|e| format!("audio device notifications: {e}"))?;
        Ok(DefaultRenderWatch {
            enumerator,
            client,
            _com: com,
        })
    }
}

impl Drop for DefaultRenderWatch {
    fn drop(&mut self) {
        // SAFETY: unregisters the same interface pointer `new` registered, while both it and
        // the enumerator are still alive. After this returns Windows delivers no further
        // callbacks, so the closure's captures are free to go with the object.
        if let Err(e) = unsafe {
            self.enumerator
                .UnregisterEndpointNotificationCallback(&self.client)
        } {
            // Nothing left to do about it, but a registration that survived its object would
            // explain a later crash in a WASAPI thread, so it is not swallowed.
            tracing::warn!(error = %e, "audio device notifications not unregistered");
        }
    }
}

/// This thread's COM apartment, as far as `DefaultRenderWatch` is responsible for it.
struct Apartment {
    /// True when this guard's own `CoInitializeEx` is the one that has to be undone.
    leave: bool,
}

impl Apartment {
    fn enter() -> Result<Apartment, String> {
        // SAFETY: initialises the calling thread's apartment; balanced in `Drop`.
        let hr = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
        // `RPC_E_CHANGED_MODE` means the thread is already in a multithreaded apartment. The
        // enumerator works there too, and that apartment is not ours to leave.
        if hr == RPC_E_CHANGED_MODE {
            return Ok(Apartment { leave: false });
        }
        // S_OK *and* S_FALSE (already initialised, same mode) each take a reference that must
        // be given back, so both arrive here as `leave: true`.
        //
        // In the daemon this is always the S_FALSE branch: cpal initialises COM on the
        // event-loop thread the first time any device call runs there (`cpal::host::wasapi::com`
        // — `CpalCapture::open` in the daemon's startup is the first such call), and holds that
        // reference in a `thread_local!` released only at thread exit. winit's `OleInitialize`
        // is inside window creation and never runs when `indicator.pill = false`; `tray-icon`
        // initialises no COM of its own. Either way the order here is not load-bearing:
        // `CoInitializeEx`/`CoUninitialize` are reference-counted per thread, so this guard
        // only ever gives back the one reference it took.
        hr.ok().map_err(|e| format!("COM apartment: {e}"))?;
        Ok(Apartment { leave: true })
    }
}

impl Drop for Apartment {
    fn drop(&mut self) {
        if self.leave {
            // SAFETY: balances the `CoInitializeEx` in `enter`, on the same thread.
            unsafe { CoUninitialize() };
        }
    }
}

/// The COM object Windows calls back into. It holds nothing but the callback: every method
/// here runs on a WASAPI thread, so any state it touched would need its own synchronisation.
#[implement(IMMNotificationClient)]
struct Notify {
    on_change: Box<dyn Fn() + Send + Sync>,
}

impl IMMNotificationClient_Impl for Notify_Impl {
    /// Output only, and only the roles the default output device is resolved through: cpal
    /// asks for `eConsole`, and `eMultimedia` moves with it on every ordinary switch.
    fn OnDefaultDeviceChanged(&self, flow: EDataFlow, role: ERole, _id: &PCWSTR) -> ComResult<()> {
        if flow == eRender && (role == eConsole || role == eMultimedia) {
            (self.on_change)();
        }
        Ok(())
    }

    /// Fires for capture endpoints too, so the id is screened first — see `is_capture`.
    fn OnDeviceRemoved(&self, id: &PCWSTR) -> ComResult<()> {
        if !is_capture(id) {
            (self.on_change)();
        }
        Ok(())
    }

    /// The disconnect path proper: a headset going away is a state change to `NOTPRESENT` or
    /// `UNPLUGGED` before it is anything else.
    fn OnDeviceStateChanged(&self, id: &PCWSTR, _state: DEVICE_STATE) -> ComResult<()> {
        if !is_capture(id) {
            (self.on_change)();
        }
        Ok(())
    }

    /// A device appearing does not move the default on its own; if it does, the
    /// `OnDefaultDeviceChanged` that follows is the notification that matters.
    fn OnDeviceAdded(&self, _id: &PCWSTR) -> ComResult<()> {
        Ok(())
    }

    /// Volume and friendly-name edits leave an open stream alone; a shared-mode **format**
    /// change does not. Sound panel → device Properties → Advanced → Default Format
    /// reconfigures the audio engine and hands every client bound to that endpoint
    /// `AUDCLNT_E_DEVICE_INVALIDATED` — without moving the default and without touching
    /// `DEVICE_STATE`, so this is the *only* notification it fires. Left out, it is issue #7
    /// again by a second route: the sink dies where `Mixer::add` cannot see it.
    fn OnPropertyValueChanged(&self, id: &PCWSTR, key: &PROPERTYKEY) -> ComResult<()> {
        if *key == PKEY_AudioEngine_DeviceFormat && !is_capture(id) {
            (self.on_change)();
        }
        Ok(())
    }
}

/// The prefix every WASAPI *capture* endpoint id carries. Render endpoints use `{0.0.0.`.
const CAPTURE_ID_PREFIX: &str = "{0.0.1.";

/// Whether an endpoint id names a capture device, so the caller can skip it.
///
/// Three of the five notifications carry no data flow, and the documented way to ask for one —
/// `IMMDeviceEnumerator::GetDevice` then `IMMEndpoint::GetDataFlow` — is a call back into the
/// audio APIs from inside a callback, which is the one thing Windows says not to do. The id
/// string carries it instead: WASAPI endpoint ids are `{0.0.0.00000000}.{guid}` for render and
/// `{0.0.1.00000000}.{guid}` for capture.
///
/// That format is a convention rather than a contract, so only the exact capture prefix counts
/// as known. Anything else — a null id, a scheme Windows changes one day — reads as "not
/// capture" and fires a re-open, which is what this code did before the screen existed. The
/// cost of being wrong is therefore a redundant re-open, never a missed one.
///
/// No allocation and no failure path: this runs on a WASAPI thread, where a panic would unwind
/// out of an `extern "system"` frame and abort the process.
fn is_capture(id: &PCWSTR) -> bool {
    if id.is_null() {
        return false;
    }
    // SAFETY: Windows passes a NUL-terminated id that outlives the callback.
    let wide = unsafe { id.as_wide() };
    wide.len() >= CAPTURE_ID_PREFIX.len()
        && wide
            .iter()
            .zip(CAPTURE_ID_PREFIX.bytes())
            .all(|(&w, b)| w == u16::from(b))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A NUL-terminated UTF-16 buffer to point a `PCWSTR` at. Returned rather than borrowed so
    /// the caller keeps it alive for the length of the call.
    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// The screen that keeps a microphone being plugged in from re-binding the cue sink. It
    /// has to err towards firing: everything it does not positively recognise as capture must
    /// read as "might be the render default", or a missed change is #7 again.
    #[test]
    fn only_a_recognised_capture_id_is_screened_out() {
        let capture = wide("{0.0.1.00000000}.{04385bed-c11a-4133-ae99-9468e4b0a8de}");
        assert!(is_capture(&PCWSTR(capture.as_ptr())));

        for not_capture in [
            // A render endpoint, the case that must always fire.
            "{0.0.0.00000000}.{94d19b7c-5433-4505-bb22-50b69bb593ba}",
            // Loopback capture on a *render* endpoint keeps the render prefix.
            "{0.0.0.00000000}.{f92bcf48-abec-4907-aea1-1082698e885c}",
            // Anything unrecognised, including a scheme Windows might change.
            "",
            "{",
            "{0.0.1",
            "\\\\?\\SWD#MMDEVAPI#somethingelse",
        ] {
            let buf = wide(not_capture);
            assert!(
                !is_capture(&PCWSTR(buf.as_ptr())),
                "`{not_capture}` was screened out; only a known capture id may be"
            );
        }

        // A null id is what a notification carries when Windows has no id to give.
        assert!(!is_capture(&PCWSTR::null()));
    }

    /// `#[ignore]`d because it reaches the real audio service: on a box without one — a CI
    /// runner — `CoCreateInstance` fails at the class, not at anything this is about. Run it
    /// on a desktop with `cargo test -- --ignored`.
    ///
    /// It is the registration itself that is under test, and the apartment book-keeping
    /// behind it. A second watch after the first has gone is the assertion that matters: it
    /// can only succeed if `Drop` left the apartment exactly as balanced as it found it —
    /// one `CoUninitialize` too many would have torn COM down under this thread, and the
    /// second `CoCreateInstance` would come back `CO_E_NOTINITIALIZED`.
    #[test]
    #[ignore]
    fn a_watch_registers_and_leaves_the_apartment_as_it_found_it() {
        let watch = DefaultRenderWatch::new(|| {}).expect("first registration");
        drop(watch);
        let again = DefaultRenderWatch::new(|| {}).expect("registration after a clean drop");
        // Two live at once: `Register` takes each client separately, so this is what a
        // re-registration on the same thread has to survive.
        let overlapping = DefaultRenderWatch::new(|| {}).expect("a second live registration");
        drop(again);
        drop(overlapping);
    }
}
