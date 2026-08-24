//! Keyboard-layout read for the foreground window, normalised to ISO 639-1.

use crate::lang::Lang;

pub trait Layout: Send {
    fn current(&self) -> Option<Lang>;
}

/// The rung used when no backend can read layouts.
pub struct NoLayout;
impl Layout for NoLayout {
    fn current(&self) -> Option<Lang> {
        None
    }
}
