use embed_manifest::manifest::DpiAwareness;
use embed_manifest::{embed_manifest, new_manifest};

fn main() {
    if std::env::var_os("CARGO_CFG_WINDOWS").is_some() {
        embed_manifest(new_manifest("Baduhan.App").dpi_awareness(DpiAwareness::PerMonitorV2))
            .expect("unable to embed manifest");
    }
    println!("cargo:rustc-env=BADUHAN_GIT_DESC={}", git_desc());
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/index");
    println!("cargo:rerun-if-changed=build.rs");
}

/// Short commit hash, "-dirty" suffixed when the tree has uncommitted
/// changes; "unknown" outside a git checkout (e.g. a crates.io build).
fn git_desc() -> String {
    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .args(args)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
    };
    let Some(hash) = git(&["rev-parse", "--short", "HEAD"]) else {
        return "unknown".into();
    };
    let dirty = git(&["status", "--porcelain", "--untracked-files=no"])
        .is_none_or(|s| !s.is_empty());
    if dirty { format!("{hash}-dirty") } else { hash }
}
