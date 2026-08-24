//! Microphone capture trait: open on press, closed on release, never held open.

use crate::audio::Audio;

pub trait Capture: Send {
    fn start(&mut self) -> Result<(), String>;
    fn stop(&mut self) -> Result<Audio, String>;
}
