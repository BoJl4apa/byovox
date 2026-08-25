//! Recording indication: tray colour always; pill and cue as configured layers.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IndicatorState {
    Idle,
    Recording,
    Working,
    /// A dictation that reached the focused window. Painted exactly like Idle — it is idle —
    /// but it is the only state that plays the done cue. Spec §Pipeline detail 2 and 5: a
    /// sub-`min_hold` tap is discarded *silently* and an empty transcript gets *no cue*, and
    /// a cancelled recording must not sound like a successful one.
    Done,
    /// Shown for ~3 s by the UI, then Idle; the pipeline never sets Idle after Error.
    Error,
}

pub trait Indicator: Send {
    fn set(&mut self, state: IndicatorState);
}
