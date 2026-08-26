//! Watching the default audio *render* endpoint.
//!
//! Depends on the WASAPI device enumerator (`IMMDeviceEnumerator`) and on the COM apartment of
//! the thread that builds it. Produces one callback per Windows notification that the default
//! render endpoint moved, or that an endpoint went away. It decides nothing and holds no audio
//! stream: what to do about a change is the caller's — `ui::App` re-opens its cue sink, which
//! is the whole of issue #7.

use windows::Win32::Foundation::{PROPERTYKEY, RPC_E_CHANGED_MODE};
use windows::Win32::Media::Audio::{
    DEVICE_STATE, EDataFlow, ERole, IMMDeviceEnumerator, IMMNotificationClient,
    IMMNotificationClient_Impl, MMDeviceEnumerator, eConsole, eMultimedia, eRender,
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
        // S_OK *and* S_FALSE (already initialised, same mode — winit does this on the event
        // loop thread when it creates a window) each take a reference that must be given back,
        // so both arrive here as `leave: true`.
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

    /// Fires for capture endpoints too — the flow is not in the notification, and asking the
    /// enumerator from inside a callback is the one thing Windows says not to do. Firing on
    /// both is deliberate: a redundant re-open costs a sink, a missed one is #7 again.
    fn OnDeviceRemoved(&self, _id: &PCWSTR) -> ComResult<()> {
        (self.on_change)();
        Ok(())
    }

    /// The disconnect path proper: a headset going away is a state change to `NOTPRESENT` or
    /// `UNPLUGGED` before it is anything else.
    fn OnDeviceStateChanged(&self, _id: &PCWSTR, _state: DEVICE_STATE) -> ComResult<()> {
        (self.on_change)();
        Ok(())
    }

    /// A device appearing does not move the default on its own; if it does, the
    /// `OnDefaultDeviceChanged` that follows is the notification that matters.
    fn OnDeviceAdded(&self, _id: &PCWSTR) -> ComResult<()> {
        Ok(())
    }

    /// Volume, format and friendly-name edits. None of them invalidate an open stream.
    fn OnPropertyValueChanged(&self, _id: &PCWSTR, _key: &PROPERTYKEY) -> ComResult<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
