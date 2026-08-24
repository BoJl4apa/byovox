//! byovox — push-to-talk dictation against a speech-to-text server you run.
//!
//! Every module lives here; `src/main.rs` is a thin binary over this library.

pub mod audio;
pub mod config;
pub mod lang;
pub mod multipart;
pub mod polish;
pub mod stt;
#[cfg(test)]
pub mod testutil;
