#![cfg(unix)]

use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    process::{Command, Output},
};

struct Harness {
    root: tempfile::TempDir,
    config: PathBuf,
    runtime: PathBuf,
}

impl Harness {
    fn new() -> Self {
        let root = tempfile::Builder::new()
            .prefix("brs-test-")
            .tempdir_in("/tmp")
            .unwrap();
        let config = root.path().join("config.toml");
        fs::write(
            &config,
            "[[tunnels]]\nname='dev'\nhost='server'\nlocal=9000\nremote='localhost:80'\n",
        )
        .unwrap();
        let runtime = root.path().join("run");
        Self {
            root,
            config,
            runtime,
        }
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_boring-rs"));
        command
            .env("BORING_CONFIG", &self.config)
            .env("BORING_RUNTIME_DIR", &self.runtime)
            .env_remove("BORING_SOCK")
            .env_remove("BORING_LOG_FILE");
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command().args(args).output().unwrap()
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = self.run(&["shutdown"]);
    }
}

#[test]
fn dry_run_needs_no_ssh_or_daemon() {
    let test = Harness::new();
    let result = test
        .command()
        .env("PATH", "")
        .args(["open", "dev", "--dry-run"])
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(String::from_utf8_lossy(&result.stdout).contains("127.0.0.1:9000:localhost:80"));
    assert!(!test.runtime.exists());
}

#[test]
fn init_never_overwrites_user_config() {
    let test = Harness::new();
    let original = fs::read(&test.config).unwrap();
    assert!(!test.run(&["init"]).status.success());
    assert_eq!(fs::read(&test.config).unwrap(), original);
    fs::remove_file(&test.config).unwrap();
    assert!(test.run(&["init"]).status.success());
    assert_eq!(
        fs::metadata(&test.config).unwrap().permissions().mode() & 0o777,
        0o600
    );
}

#[test]
fn malformed_selection_does_not_start_manager() {
    let test = Harness::new();
    for args in [
        vec!["open"],
        vec!["open", "["],
        vec!["open", "--all", "dev"],
        vec!["open", "--group", "missing"],
    ] {
        assert!(!test.run(&args).status.success(), "{args:?}");
    }
    assert!(!test.runtime.exists());
}

#[test]
fn missing_ssh_returns_failure_and_manager_can_shutdown() {
    let test = Harness::new();
    let result = test
        .command()
        .env("PATH", "")
        .args(["open", "dev"])
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("cannot start OpenSSH"));
    assert!(test.run(&["shutdown"]).status.success());
    assert!(!test.runtime.join("daemon.sock").exists());
}

#[test]
fn ssh_diagnostics_are_returned_and_no_retry_on_initial_failure() {
    let test = Harness::new();
    let ssh = test.root.path().join("ssh");
    fs::write(
        &ssh,
        "#!/bin/sh\necho 'fixture: authentication failed' >&2\nexit 23\n",
    )
    .unwrap();
    fs::set_permissions(&ssh, fs::Permissions::from_mode(0o700)).unwrap();
    let result = test
        .command()
        .env("PATH", test.root.path())
        .args(["open", "dev"])
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("fixture: authentication failed"));
    let entries: serde_json::Value =
        serde_json::from_slice(&test.run(&["list", "--json"]).stdout).unwrap();
    assert_eq!(entries[0]["status"], "failed");
    assert_eq!(entries[0]["retries"], 0);
}
