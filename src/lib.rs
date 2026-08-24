//! byovox — push-to-talk dictation against a speech-to-text server you run.
//!
//! Every module lives here; `src/main.rs` is a thin binary over this library.

pub mod audio;
pub mod capture;
pub mod config;
pub mod hotkey;
pub mod indicator;
pub mod inject;
pub mod lang;
pub mod layout;
pub mod multipart;
pub mod pipeline;
pub mod polish;
pub mod stt;
#[cfg(test)]
pub mod testutil;
