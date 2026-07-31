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
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock};

/// Path to the compiled `devc` binary (provided by Cargo for integration tests).
fn devc_bin() -> &'static str {
    env!("CARGO_BIN_EXE_devc")
}

/// The repo's `test-workspaces` directory.
fn workspaces_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("test-workspaces")
}

/// Run `devc` in the given workspace subdirectory (under `test-workspaces/`) with `args` and
/// `stdin`, capturing its output.
fn run(workspace: &str, args: &[&str], stdin: &str) -> Output {
    run_in_dir(&workspaces_dir().join(workspace), args, stdin)
}

/// Run `devc` in an arbitrary absolute directory with `args` and `stdin`, capturing its output.
/// Used by walk-up tests that need to run outside `test-workspaces/` (e.g. an isolated temp tree).
fn run_in_dir(dir: &Path, args: &[&str], stdin: &str) -> Output {
    let mut child = Command::new(devc_bin())
        .args(args)
        .current_dir(dir)
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

/// A unique temporary directory tree, removed on drop. Used to isolate "no spec found" tests from
/// the repo's own ancestors: walk-up climbs to `/`, so a spec added anywhere above `test-workspaces/`
/// (the repo root, `$HOME`, …) would otherwise perturb these tests.
struct TempTree {
    base: PathBuf,
}

impl TempTree {
    /// Create a fresh base dir under the OS temp dir with the nested subpath `rel` created inside it.
    fn new(rel: &str) -> TempTree {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let base = std::env::temp_dir().join(format!("devc-test-{}-{}", std::process::id(), n));
        std::fs::create_dir_all(base.join(rel)).expect("create temp tree");
        TempTree { base }
    }

    /// The deepest directory (`base/rel`) to run `devc` from.
    fn deep(&self, rel: &str) -> PathBuf {
        self.base.join(rel)
    }
}

impl Drop for TempTree {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.base);
    }
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
    // Run in an isolated temp dir: walk-up climbs to `/`, so running inside the repo would couple
    // this assertion to every ancestor (repo root, $HOME, …).
    let tree = TempTree::new("");
    let out = run_in_dir(&tree.base, &[], "");
    assert!(!out.status.success(), "should fail without a config");
    assert!(
        stderr(&out).contains("No dev container configuration found"),
        "should explain the missing config, got: {}",
        stderr(&out)
    );
}

#[test]
fn missing_config_walks_up_then_errors() {
    // From a deep subdirectory with no spec in any ancestor of the temp tree, discovery walks up
    // and reports that it searched parents too.
    let tree = TempTree::new("a/b/c");
    let out = run_in_dir(&tree.deep("a/b/c"), &[], "");
    assert!(!out.status.success(), "should fail without a config anywhere above");
    let e = stderr(&out);
    assert!(
        e.contains("No dev container configuration found"),
        "should explain the missing config, got: {e}"
    );
    assert!(
        e.contains("or any parent directory"),
        "should mention it walked up, got: {e}"
    );
}

#[test]
fn config_is_found_from_subdirectory() {
    require_docker!();
    let _guard = docker_lock();
    let ws = "image-simple";
    cleanup(ws);

    // Running from a subdirectory of a workspace, discovery should climb to the spec (image-simple)
    // rather than reporting not-found. Docker-gated + locked because a successful climb brings the
    // container up, which would otherwise pollute the shared image-simple tests.
    let out = run("image-simple/nested/deep", &[], "exit 0\n");
    assert!(out.status.success(), "run should succeed; stderr: {}", stderr(&out));
    assert!(
        !stderr(&out).contains("No dev container configuration found"),
        "discovery should climb to image-simple from a subdir, got: {}",
        stderr(&out)
    );

    cleanup(ws);
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
fn walks_up_to_workspace_root_from_subdirectory() {
    require_docker!();
    let _guard = docker_lock();
    let ws = "image-simple";
    cleanup(ws);

    // Invoked from image-simple/nested/deep, the workspace root (mount + /workspaces/<name>) must be
    // image-simple itself, not the subdirectory. The shell's cwd moves into the subdir (covered by
    // shell_opens_in_invocation_subdirectory), so a relative `../../hello.txt` from `nested/deep`
    // must climb back to the root marker — proving both the cwd depth and that the mount covers root.
    let out = run("image-simple/nested/deep", &[], "cat ../../hello.txt\nexit 0\n");
    let o = stdout(&out);
    let e = stderr(&out);

    assert!(out.status.success(), "run should succeed; stderr: {e}");
    // The bind mount is the whole workspace, so the marker file at the root is visible.
    assert!(o.contains("bind-mount marker"), "bind mount should cover workspace root; stdout: {o}");
    // The container is labeled for image-simple (same identity as running from the root).
    assert_eq!(container_ids(ws).len(), 1, "exactly one labeled container for the workspace root");

    cleanup(ws);
}

#[test]
fn shell_opens_in_invocation_subdirectory() {
    require_docker!();
    let _guard = docker_lock();
    let ws = "image-simple";
    cleanup(ws);

    // Invoked from a subdirectory, the interactive shell should open in that subdirectory (mapped
    // into the container), not at the workspace root.
    let out = run("image-simple/nested/deep", &[], "pwd\nexit 0\n");
    let o = stdout(&out);
    let e = stderr(&out);

    assert!(out.status.success(), "run should succeed; stderr: {e}");
    assert!(
        o.contains("/workspaces/image-simple/nested/deep"),
        "shell cwd should be the invocation subdir; stdout: {o}\nstderr: {e}"
    );

    cleanup(ws);
}

#[test]
fn command_runs_in_invocation_subdirectory() {
    require_docker!();
    let _guard = docker_lock();
    let ws = "image-simple";
    cleanup(ws);

    // The `devc <command>` form is offset to the invocation subdir too, not just the shell.
    let out = run("image-simple/nested/deep", &["pwd"], "");
    let o = stdout(&out);
    let e = stderr(&out);

    assert!(out.status.success(), "run should succeed; stderr: {e}");
    assert!(
        o.contains("/workspaces/image-simple/nested/deep"),
        "command cwd should be the invocation subdir; stdout: {o}\nstderr: {e}"
    );

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

#[test]
fn dockerfile_is_resolved_relative_to_config_not_context() {
    require_docker!();
    let _guard = docker_lock();
    let ws = "dockerfile-context";
    cleanup(ws);

    // The config has `"dockerfile": "Dockerfile"` with `"context": ".."`. Per the spec, the
    // Dockerfile is resolved relative to devcontainer.json (.devcontainer/Dockerfile), not relative
    // to the context (which holds a decoy Dockerfile at the workspace root).
    let out = run(ws, &[], "cat /which-dockerfile.txt\nexit 0\n");
    let o = stdout(&out);
    let e = stderr(&out);

    assert!(out.status.success(), "run should succeed; stderr: {e}");
    assert!(e.contains("building image"), "should build the image; stderr: {e}");
    assert!(
        o.contains("correct-dockerfile-in-devcontainer"),
        "should build .devcontainer/Dockerfile, not the context-root decoy; stdout: {o}\nstderr: {e}"
    );
    assert!(
        !o.contains("wrong-dockerfile-at-context-root"),
        "must not build the Dockerfile at the context root; stdout: {o}"
    );

    cleanup(ws);
}

/// Full Dev Container Features flow: fetch two OCI features, build the extended image, and verify the
/// installed tools are on PATH inside the container. Requires Docker *and* network access to
/// ghcr.io / the feature downloads. Fast once the image layers are cached; Docker-gated so it
/// self-skips without a daemon.
#[test]
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

/// An unreadable `.devcontainer` directory encountered during the walk is reported to stderr and the
/// walk continues rather than aborting.
#[test]
#[cfg(unix)]
fn permission_denied_ancestor_warns_and_continues() {
    use std::os::unix::fs::PermissionsExt;

    let tree = TempTree::new("a/b/c");
    // A `.devcontainer` dir at the top of the tree that we can't enter: stat of the nested
    // devcontainer.json fails with EACCES.
    let blocked = tree.base.join(".devcontainer");
    std::fs::create_dir_all(&blocked).expect("create .devcontainer");
    std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o000)).expect("chmod 000");

    let out = run_in_dir(&tree.deep("a/b/c"), &[], "");

    // Restore permissions so the TempTree can be removed on drop.
    let _ = std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o755));

    let e = stderr(&out);
    assert!(!out.status.success(), "should still fail without a usable config; stderr: {e}");
    assert!(
        e.contains("warning: cannot check"),
        "should warn about the unreadable directory, got: {e}"
    );
    assert!(
        e.contains("No dev container configuration found"),
        "walk should continue and report not-found, got: {e}"
    );
}
