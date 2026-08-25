//! Build script: stamps the commit this binary was built from into `BYOVOX_GIT_SHA`.
//!
//! Depends on `git` being on PATH and the source being a checkout; produces one
//! `rustc-env` that `byovox::VERSION` reads. Neither condition is required — a crates.io
//! tarball has no `.git` and a build container may have no git at all — so every failure
//! here becomes `unknown` rather than a build error. A version string that cannot name a
//! commit is a smaller problem than a crate that will not compile.

fn main() {
    // What a commit actually rewrites is the ref HEAD points at — `.git/HEAD` is a symbolic
    // ref whose mtime moves only on a checkout or a branch switch. Watching it alone left
    // the stamp naming the *previous* commit after every local commit, which is worse than
    // `unknown`: confidently wrong, and it sends a bug report to the wrong tree. So watch
    // all three places the answer can move: HEAD itself, the ref it names, and `packed-refs`,
    // where that ref lives once git has packed it away.
    //
    // A path that does not exist — a ref that is only packed, or a linked worktree, where
    // `.git` is a file and none of these resolve — cargo treats as always-changed, so the
    // script simply re-runs every build there. Conservative, and the stamp stays correct.
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/packed-refs");
    if let Ok(head) = std::fs::read_to_string(".git/HEAD")
        && let Some(git_ref) = head.strip_prefix("ref: ")
    {
        println!("cargo:rerun-if-changed=.git/{}", git_ref.trim());
    }

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
