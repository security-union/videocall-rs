fn main() {
    // Prefer env vars (set by Docker ARGs during CI builds) over git commands (local dev).
    let sha = std::env::var("GIT_SHA")
        .map(|s| s.chars().take(8).collect())
        .unwrap_or_else(|_| git_short("rev-parse", &["--short", "HEAD"]));
    let branch = std::env::var("GIT_BRANCH")
        .unwrap_or_else(|_| git_short("rev-parse", &["--abbrev-ref", "HEAD"]));
    let ts = std::env::var("BUILD_TIMESTAMP").unwrap_or_else(|_| now_utc());

    println!("cargo:rustc-env=GIT_SHA={sha}");
    println!("cargo:rustc-env=GIT_BRANCH={branch}");
    println!("cargo:rustc-env=BUILD_TIMESTAMP={ts}");

    println!("cargo:rerun-if-changed=../.git/HEAD");
    println!("cargo:rerun-if-env-changed=GIT_SHA");
    println!("cargo:rerun-if-env-changed=GIT_BRANCH");
    println!("cargo:rerun-if-env-changed=BUILD_TIMESTAMP");
}

fn git_short(cmd: &str, args: &[&str]) -> String {
    std::process::Command::new("git")
        .arg(cmd)
        .args(args)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "unknown".into())
}

fn now_utc() -> String {
    // Avoid adding chrono as a build-dep; shell `date` is fine.
    std::process::Command::new("date")
        .args(["-u", "+%Y-%m-%dT%H:%M:%SZ"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "unknown".into())
}
