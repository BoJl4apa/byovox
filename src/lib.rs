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
pub mod setup;
pub mod stt;
#[cfg(test)]
pub mod testutil;
pub mod ui;

/// What `--version` prints and the daemon logs: `0.1.0 (8169c85)`.
///
/// The package version alone names a release; between releases it names every commit since
/// the last one, which is the range a bug report would otherwise have to be searched over.
/// `build.rs` supplies the commit, or `unknown` where there is no checkout to read.
pub const VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), " (", env!("BYOVOX_GIT_SHA"), ")");

#[cfg(test)]
mod tests {
    use super::VERSION;

    /// The shape is the interface: a release workflow checks the tag against the package
    /// version, and anyone reading an issue expects the commit in the parentheses.
    #[test]
    fn version_is_the_package_version_then_a_commit() {
        let sha = VERSION
            .strip_prefix(concat!(env!("CARGO_PKG_VERSION"), " ("))
            .and_then(|rest| rest.strip_suffix(')'))
            .unwrap_or_else(|| panic!("expected `<version> (<sha>)`, got `{VERSION}`"));
        assert!(
            sha == "unknown" || (!sha.is_empty() && sha.chars().all(|c| c.is_ascii_hexdigit())),
            "expected a hex commit or `unknown`, got `{sha}`"
        );
    }
}
