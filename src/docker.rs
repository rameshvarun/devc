//! Thin wrappers around the `docker` CLI. Like the reference Dev Containers CLI, devc drives the
//! container runtime by shelling out to the `docker` binary rather than talking to the daemon API.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{anyhow, bail, Context, Result};

/// A container matched by its devc/devcontainer labels.
pub struct FoundContainer {
    pub id: String,
    pub running: bool,
}

/// Run `docker` with `args`, inheriting stdio, and return an error if it exits non-zero.
fn run_inherit(args: &[String]) -> Result<()> {
    let status = Command::new("docker")
        .args(args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .context("failed to launch `docker` (is it installed and on PATH?)")?;
    if !status.success() {
        bail!("docker {} failed with {}", args.join(" "), status);
    }
    Ok(())
}

/// Run `docker` with `args`, capturing stdout (trimmed). stderr is inherited so progress/errors show.
fn run_capture(args: &[String]) -> Result<String> {
    let output = Command::new("docker")
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::inherit())
        .output()
        .context("failed to launch `docker` (is it installed and on PATH?)")?;
    if !output.status.success() {
        bail!("docker {} failed with {}", args.join(" "), output.status);
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Build an image from a Dockerfile, tagging it `tag`. `context` and `dockerfile` are absolute paths.
#[allow(clippy::too_many_arguments)]
pub fn build(
    context: &Path,
    dockerfile: &Path,
    tag: &str,
    build_args: &BTreeMap<String, String>,
    target: Option<&str>,
    cache_from: &[String],
    extra_options: &[String],
) -> Result<()> {
    let mut args = vec![
        "build".to_string(),
        "-f".to_string(),
        dockerfile.to_string_lossy().into_owned(),
        "-t".to_string(),
        tag.to_string(),
    ];
    for (k, v) in build_args {
        args.push("--build-arg".to_string());
        args.push(format!("{k}={v}"));
    }
    if let Some(t) = target {
        args.push("--target".to_string());
        args.push(t.to_string());
    }
    for c in cache_from {
        args.push("--cache-from".to_string());
        args.push(c.clone());
    }
    args.extend(extra_options.iter().cloned());
    args.push(context.to_string_lossy().into_owned());
    run_inherit(&args)
}

/// `docker run` the given fully-formed args (after the `run` verb) and return the new container id.
pub fn run(run_args: &[String]) -> Result<String> {
    let mut args = vec!["run".to_string()];
    args.extend(run_args.iter().cloned());
    let id = run_capture(&args)?;
    if id.is_empty() {
        bail!("docker run did not return a container id");
    }
    Ok(id)
}

/// Find a container carrying the given label key=value pairs. Returns the most recent match.
pub fn find_by_labels(labels: &[(&str, &str)]) -> Result<Option<FoundContainer>> {
    let mut args = vec![
        "ps".to_string(),
        "-a".to_string(),
        "--no-trunc".to_string(),
        "--format".to_string(),
        "{{.ID}}\t{{.State}}".to_string(),
    ];
    for (k, v) in labels {
        args.push("--filter".to_string());
        args.push(format!("label={k}={v}"));
    }
    let out = run_capture(&args)?;
    let line = match out.lines().next() {
        Some(l) if !l.trim().is_empty() => l,
        _ => return Ok(None),
    };
    let mut fields = line.split('\t');
    let id = fields
        .next()
        .ok_or_else(|| anyhow!("unexpected docker ps output: {line:?}"))?
        .to_string();
    let state = fields.next().unwrap_or("");
    Ok(Some(FoundContainer {
        id,
        running: state == "running",
    }))
}

/// Start a stopped container.
pub fn start(id: &str) -> Result<()> {
    run_inherit(&["start".to_string(), id.to_string()])
}

/// Run a command inside a container.
///
/// `interactive` attaches our stdin to the command (`-i`, stdin inherited) — used for the shell;
/// lifecycle commands pass `false` so they never consume the user's stdin. `tty` adds `-t` to
/// allocate a pseudo-terminal, which docker only permits when our stdin is a real TTY (matching the
/// reference CLI's behavior). stdout/stderr are always inherited.
pub fn exec(
    id: &str,
    user: Option<&str>,
    cwd: Option<&str>,
    env: &[(String, String)],
    cmd: &[String],
    interactive: bool,
    tty: bool,
) -> Result<std::process::ExitStatus> {
    let mut args = vec!["exec".to_string()];
    if interactive {
        args.push("-i".to_string());
    }
    if tty {
        args.push("-t".to_string());
    }
    if let Some(u) = user {
        if !u.is_empty() {
            args.push("-u".to_string());
            args.push(u.to_string());
        }
    }
    for (k, v) in env {
        args.push("-e".to_string());
        args.push(format!("{k}={v}"));
    }
    if let Some(w) = cwd {
        args.push("-w".to_string());
        args.push(w.to_string());
    }
    args.push(id.to_string());
    args.extend(cmd.iter().cloned());

    let stdin = if interactive {
        Stdio::inherit()
    } else {
        Stdio::null()
    };
    let status = Command::new("docker")
        .args(&args)
        .stdin(stdin)
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .context("failed to launch `docker exec`")?;
    Ok(status)
}
