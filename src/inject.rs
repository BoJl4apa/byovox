//! Text insertion rungs. The pipeline tries them in order; the last rung is `none`.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InjectMode {
    Auto,
    Type,
    Paste,
    ClipboardOnly,
}

impl InjectMode {
    pub fn parse(s: &str) -> Option<InjectMode> {
        match s {
            "auto" => Some(InjectMode::Auto),
            "type" => Some(InjectMode::Type),
            "paste" => Some(InjectMode::Paste),
            "clipboard-only" => Some(InjectMode::ClipboardOnly),
            _ => None,
        }
    }
}

pub trait Inject: Send {
    /// Rung name for logs and `check`: "type", "paste", "clipboard-only".
    fn name(&self) -> &'static str;
    fn inject(&mut self, text: &str) -> Result<(), String>;
}
