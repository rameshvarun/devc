//! End-to-end tests that drive the `devc` binary as a black box against the sample workspaces in
//! `test-workspaces/`.
//!
//! Tests that actually create containers require a working Docker daemon; they are skipped (with a
//! printed note) when Docker is unavailable, so the suite still passes in environments without it.
//! Because several workspaces map to a single container identity (keyed on the workspace path), the
//! Docker-touching tests serialize on a shared lock and clean up their container before and after.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{Mutex, MutexGuard, OnceLock};

/// Path to the compiled `devc` binary (provided by Cargo for integration tests).
fn devc_bin() -> &'static str {
    env!("CARGO_BIN_EXE_devc")
}

/// The repo's `test-workspaces` directory.
fn workspaces_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("test-workspaces")
}

/// Run `devc` in the given workspace subdirectory with `args` and `stdin`, capturing its output.
fn run(workspace: &str, args: &[&str], stdin: &str) -> Output {
    let dir = workspaces_dir().join(workspace);
    let mut child = Command::new(devc_bin())
        .args(args)
        .current_dir(&dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn devc");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(stdin.as_bytes())
        .expect("write stdin");
    child.wait_with_output().expect("failed to wait on devc")
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// Whether a Docker daemon is reachable.
fn docker_available() -> bool {
    Command::new("docker")
        .arg("info")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Serialize Docker-touching tests (they share workspace-keyed containers and the daemon).
fn docker_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

/// Remove any container devc created for the given workspace (matched by its label).
fn cleanup(workspace: &str) {
    let dir = std::fs::canonicalize(workspaces_dir().join(workspace)).expect("canonicalize");
    let out = Command::new("docker")
        .args([
            "ps",
            "-aq",
            "--filter",
            &format!("label=devcontainer.local_folder={}", dir.display()),
        ])
        .output();
    if let Ok(out) = out {
        for id in String::from_utf8_lossy(&out.stdout).split_whitespace() {
            let _ = Command::new("docker").args(["rm", "-f", id]).output();
        }
    }
}

/// `docker ps` count for a workspace's label (used to assert a container exists / was reused).
fn container_ids(workspace: &str) -> Vec<String> {
    let dir = std::fs::canonicalize(workspaces_dir().join(workspace)).expect("canonicalize");
    let out = Command::new("docker")
        .args([
            "ps",
            "-aq",
            "--filter",
            &format!("label=devcontainer.local_folder={}", dir.display()),
        ])
        .output()
        .expect("docker ps");
    String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .map(|s| s.to_string())
        .collect()
}

/// Macro to skip a Docker-dependent test cleanly when no daemon is present.
macro_rules! require_docker {
    () => {
        if !docker_available() {
            eprintln!("skipping: Docker daemon not available");
            return;
        }
    };
}

// --- Tests that do not need Docker ---

#[test]
fn help_prints_usage() {
    let out = run("no-config", &["--help"], "");
    assert!(out.status.success(), "--help should exit 0");
    assert!(
        stderr(&out).contains("Usage:"),
        "help text should mention usage, got: {}",
        stderr(&out)
    );
}

#[test]
fn missing_config_errors() {
    let out = run("no-config", &[], "");
    assert!(!out.status.success(), "should fail without a config");
    assert!(
        stderr(&out).contains("No dev container configuration found"),
        "should explain the missing config, got: {}",
        stderr(&out)
    );
}

#[test]
fn compose_config_is_rejected() {
    let out = run("compose-unsupported", &[], "");
    assert!(!out.status.success(), "compose config should fail");
    assert!(
        stderr(&out).contains("docker-compose"),
        "should mention docker-compose is unsupported, got: {}",
        stderr(&out)
    );
}

// --- Docker-dependent tests ---

#[test]
fn image_up_shell_env_mount_and_exit_code() {
    require_docker!();
    let _guard = docker_lock();
    let ws = "image-simple";
    cleanup(ws);

    let out = run(
        ws,
        &[],
        "echo FOO_IS=$FOO\necho HELLO_IS=$HELLO\npwd\ncat hello.txt\nexit 7\n",
    );
    let o = stdout(&out);
    let e = stderr(&out);

    assert_eq!(out.status.code(), Some(7), "shell exit code should propagate");
    // postCreateCommand ran.
    assert!(o.contains("postcreate-ran"), "postCreate should run; stdout: {o}\nstderr: {e}");
    // containerEnv with ${localWorkspaceFolderBasename} substitution.
    assert!(o.contains("FOO_IS=bar-image-simple"), "containerEnv/substitution; stdout: {o}");
    // remoteEnv applied to the shell.
    assert!(o.contains("HELLO_IS=world"), "remoteEnv; stdout: {o}");
    // Default container workspace folder.
    assert!(o.contains("/workspaces/image-simple"), "workspace folder; stdout: {o}");
    // Workspace bind mount is visible.
    assert!(o.contains("bind-mount marker"), "bind mount; stdout: {o}");
    // A labeled container exists.
    assert_eq!(container_ids(ws).len(), 1, "exactly one labeled container");

    cleanup(ws);
}

#[test]
fn running_container_is_reused() {
    require_docker!();
    let _guard = docker_lock();
    let ws = "image-simple";
    cleanup(ws);

    let first = run(ws, &[], "exit 0\n");
    assert!(first.status.success(), "first run should succeed: {}", stderr(&first));
    assert!(
        stderr(&first).contains("creating container") || stderr(&first).contains("started container"),
        "first run should create; stderr: {}",
        stderr(&first)
    );
    let id_after_first = container_ids(ws);

    let second = run(ws, &[], "exit 0\n");
    assert!(second.status.success(), "second run should succeed");
    assert!(
        stderr(&second).contains("reusing running container"),
        "second run should reuse; stderr: {}",
        stderr(&second)
    );
    assert_eq!(container_ids(ws), id_after_first, "same container id reused");

    cleanup(ws);
}

#[test]
fn command_runs_in_container_and_propagates_exit_code() {
    require_docker!();
    let _guard = docker_lock();
    let ws = "image-simple";
    cleanup(ws);

    // A command passed as arguments runs inside the container instead of opening a shell.
    let out = run(ws, &["sh", "-c", "echo cmd-ran-in:$(pwd); exit 5"], "");
    let o = stdout(&out);
    let e = stderr(&out);

    assert_eq!(out.status.code(), Some(5), "command exit code should propagate; stderr: {e}");
    // The command runs in the container workspace folder as its cwd.
    assert!(
        o.contains("cmd-ran-in:/workspaces/image-simple"),
        "command should run in the container workspace folder; stdout: {o}\nstderr: {e}"
    );
    // A container was created for the workspace, just as the shell form does.
    assert_eq!(container_ids(ws).len(), 1, "exactly one labeled container");

    cleanup(ws);
}

#[test]
fn dockerfile_is_built_with_build_arg() {
    require_docker!();
    let _guard = docker_lock();
    let ws = "dockerfile-build";
    cleanup(ws);

    let out = run(ws, &[], "cat /greeting.txt\nexit 0\n");
    let o = stdout(&out);
    let e = stderr(&out);

    assert!(out.status.success(), "run should succeed; stderr: {e}");
    assert!(e.contains("building image"), "should build the image; stderr: {e}");
    // Build arg baked into the image, echoed by the postCreate argv command and by our `cat`.
    assert!(o.contains("hi-from-build-arg"), "build arg should apply; stdout: {o}");

    cleanup(ws);
}

/// Full Dev Container Features flow: fetch two OCI features, build the extended image, and verify the
/// installed tools are on PATH inside the container. Requires Docker *and* network access to
/// ghcr.io / the feature downloads; it is also slow (installs a JDK), so it is `#[ignore]`d by default
/// — run with `cargo test -- --ignored`.
#[test]
#[ignore = "slow: fetches OCI features and installs a JDK; needs network"]
fn features_install_java_and_gh() {
    require_docker!();
    let _guard = docker_lock();
    let ws = "features-java";
    cleanup(ws);

    let out = run(ws, &[], "java -version 2>&1\ngh --version\nwhoami\nexit 0\n");
    let o = stdout(&out);
    let e = stderr(&out);

    assert!(out.status.success(), "run should succeed; stderr: {e}");
    // Java feature installed at the requested version.
    assert!(o.contains("25.0.3"), "java 25.0.3 should be installed; stdout: {o}\nstderr: {e}");
    // github-cli feature installed.
    assert!(o.contains("gh version"), "gh should be installed; stdout: {o}");
    // Shell runs as the configured remoteUser.
    assert!(o.contains("vscode"), "should run as remoteUser vscode; stdout: {o}");

    cleanup(ws);
}
