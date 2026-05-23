//! Subprocess-level smoke test for the renamed `blueprint` binary.
//!
//! Spawns the binary in an isolated `$HOME` on a random port and exercises the
//! full publish → status → unpublish round-trip. The existing e2e/concurrent
//! tests hit the HTTP API in-process; this is the first test that actually
//! invokes the CLI binary by name, which validates the rename end to end.
//!
//! The daemon spawned by `blueprint publish` writes its lock + sqlite + log
//! into `$HOME/.blueprint/`, so pointing HOME at a `tempdir` keeps the test
//! from touching the developer's real daemon state. `BLUEPRINT_PORT=0` lets
//! the OS pick a free port.

use std::process::Command;
use std::time::Duration;
use tempfile::tempdir;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_blueprint")
}

#[test]
fn publish_status_unpublish_round_trip() {
    let home = tempdir().expect("isolated HOME");
    let html_dir = tempdir().expect("html src dir");
    let html_path = html_dir.path().join("blueprint.html");
    std::fs::write(
        &html_path,
        "<!doctype html><html><body><p>smoke test</p></body></html>",
    )
    .unwrap();
    let slug = format!("smoke-{}", std::process::id());

    let run = |args: &[&str]| {
        Command::new(bin())
            .args(args)
            .env("HOME", home.path())
            .env("BLUEPRINT_PORT", "0")
            .output()
            .expect("spawn blueprint")
    };

    let pub_out = run(&[
        "publish",
        html_path.to_str().unwrap(),
        "--slug",
        &slug,
        "--no-open",
        "--json",
    ]);
    assert!(
        pub_out.status.success(),
        "publish failed (stderr): {}",
        String::from_utf8_lossy(&pub_out.stderr)
    );
    let stdout = String::from_utf8_lossy(&pub_out.stdout);
    assert!(
        stdout.contains(&format!("\"slug\":\"{slug}\"")),
        "publish JSON didn't echo slug: {stdout}"
    );
    // The URL must use the new /b/ prefix, not the old /p/.
    assert!(
        stdout.contains("/b/"),
        "publish URL should use /b/ prefix, got: {stdout}"
    );
    assert!(
        !stdout.contains("/p/"),
        "publish URL still includes legacy /p/ prefix: {stdout}"
    );

    let status_out = run(&["status"]);
    assert!(status_out.status.success(), "status failed");
    let stdout = String::from_utf8_lossy(&status_out.stdout);
    assert!(
        stdout.contains(&slug),
        "status didn't list slug `{slug}`: {stdout}"
    );

    let unpub_out = run(&["unpublish", &slug]);
    assert!(
        unpub_out.status.success(),
        "unpublish failed (stderr): {}",
        String::from_utf8_lossy(&unpub_out.stderr)
    );

    // The daemon should have shut down because the last blueprint is gone.
    // Give it a beat to exit cleanly.
    std::thread::sleep(Duration::from_millis(300));
    let status_after = run(&["status"]);
    let stdout = String::from_utf8_lossy(&status_after.stdout);
    assert!(
        stdout.contains("daemon: not running") || !stdout.contains(&slug),
        "daemon should be stopped after unpublishing last blueprint; got: {stdout}"
    );
}
