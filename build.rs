//! Build script: stamps the commit this binary was built from into `BYOVOX_GIT_SHA`.
//!
//! Depends on `git` being on PATH and the source being a checkout; produces one
//! `rustc-env` that `byovox::VERSION` reads. Neither condition is required — a crates.io
//! tarball has no `.git` and a build container may have no git at all — so every failure
//! here becomes `unknown` rather than a build error. A version string that cannot name a
//! commit is a smaller problem than a crate that will not compile.

fn main() {
    // A commit changes HEAD, so that is what invalidates the stamp. In a linked worktree
    // `.git` is a file and this path does not exist, which cargo treats as always-changed:
    // the script re-runs every build and the stamp stays correct there too.
    println!("cargo:rerun-if-changed=.git/HEAD");

    let sha = std::process::Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|out| out.status.success())
        .and_then(|out| String::from_utf8(out.stdout).ok())
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_owned());

    println!("cargo:rustc-env=BYOVOX_GIT_SHA={sha}");
}
