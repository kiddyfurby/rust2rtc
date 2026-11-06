use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/index");

    let git_hash = std::env::var("GIT_HASH").ok().or_else(|| {
        let output = Command::new("git")
            .args(["rev-parse", "--short", "HEAD"])
            .output()
            .ok()?;
        if output.status.success() {
            String::from_utf8(output.stdout).ok().map(|s| s.trim().to_string())
        } else {
            None
        }
    }).unwrap_or_else(|| "unknown".to_string());

    println!("cargo:rustc-env=GIT_HASH={}", git_hash);
}
