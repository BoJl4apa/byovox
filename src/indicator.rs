//! Recording indication: tray colour always; pill and cue as configured layers.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IndicatorState {
    Idle,
    Recording,
    Working,
    /// Shown for ~3 s by the UI, then Idle; the pipeline never sets Idle after Error.
    Error,
}

pub trait Indicator: Send {
    fn set(&mut self, state: IndicatorState);
}
