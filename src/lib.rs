//! byovox — push-to-talk dictation against a speech-to-text server you run.
//!
//! Every module lives here, the daemon included; the two binaries are thin shells over it —
//! `src/main.rs` is the console CLI, `src/bin/byovox-daemon.rs` the windowless daemon.

pub mod audio;
pub mod capture;
pub mod capture_log;
pub mod check;
pub mod config;
pub mod daemon;
pub mod hotkey;
pub mod indicator;
pub mod inject;
pub mod ipc;
pub mod lang;
pub mod layout;
pub mod multipart;
pub mod pipeline;
pub mod platform;
pub mod polish;
pub mod stt;
#[cfg(test)]
pub mod testutil;
pub mod ui;
