//! The core `devc` flow: bring up the single-container dev container for a workspace (building or
//! reusing as needed) and open an interactive shell inside it.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};

use crate::config::{
    self, Command as LifecycleCommand, DevContainerConfig, LoadedConfig, StringOrArgv,
    SubstitutionContext,
};
use crate::docker;

const LABEL_LOCAL_FOLDER: &str = "devcontainer.local_folder";
const LABEL_CONFIG_FILE: &str = "devcontainer.config_file";
const LABEL_MANAGED: &str = "devc.managed";

/// Bring up the dev container for `workspace`, then either open an interactive shell (`command`
/// empty) or run `command` inside it. Returns the exit code devc should terminate with (the shell's
/// or the command's exit code).
pub fn up_and_run(workspace: &Path, command: &[String]) -> Result<i32> {
    let workspace = std::fs::canonicalize(workspace)
        .with_context(|| format!("resolving workspace path {}", workspace.display()))?;

    let loaded = config::load(&workspace)?;
    let config_path = std::fs::canonicalize(&loaded.config_path)
        .unwrap_or_else(|_| loaded.config_path.clone());

    // Compute workspace mount + container workspace folder, then substitute variables in the config.
    let ws_basename = basename(&workspace);
    let container_workspace_folder = loaded
        .config
        .workspace_folder
        .clone()
        .unwrap_or_else(|| format!("/workspaces/{ws_basename}"));

    let subst = SubstitutionContext {
        local_workspace_folder: workspace.to_string_lossy().into_owned(),
        local_workspace_folder_basename: ws_basename.clone(),
        container_workspace_folder: container_workspace_folder.clone(),
        container_workspace_folder_basename: basename(Path::new(&container_workspace_folder)),
    };
    let config = substitute_config(loaded.config.clone(), &subst);

    // The container workspace folder may itself contain variables; resolve after substitution.
    let container_workspace_folder = config
        .workspace_folder
        .clone()
        .unwrap_or_else(|| format!("/workspaces/{ws_basename}"));

    // initializeCommand runs on the host every time we bring the container up.
    if let Some(cmd) = &config.initialize_command {
        run_host_command(cmd, &workspace).context("initializeCommand failed")?;
    }

    let labels = [
        (LABEL_LOCAL_FOLDER, workspace.to_string_lossy().into_owned()),
        (LABEL_CONFIG_FILE, config_path.to_string_lossy().into_owned()),
    ];
    let label_refs: Vec<(&str, &str)> = labels.iter().map(|(k, v)| (*k, v.as_str())).collect();

    let existing = docker::find_by_labels(&label_refs)?;

    let container_id = match existing {
        Some(found) if found.running => {
            eprintln!("devc: reusing running container {}", short(&found.id));
            run_lifecycle_in_container(
                &found.id,
                &config,
                &container_workspace_folder,
                &[config.post_attach_command.as_ref()],
            )?;
            found.id
        }
        Some(found) => {
            eprintln!("devc: starting existing container {}", short(&found.id));
            docker::start(&found.id)?;
            run_lifecycle_in_container(
                &found.id,
                &config,
                &container_workspace_folder,
                &[
                    config.post_start_command.as_ref(),
                    config.post_attach_command.as_ref(),
                ],
            )?;
            found.id
        }
        None => create_container(
            &workspace,
            &loaded,
            &config,
            &ws_basename,
            &container_workspace_folder,
            &labels,
        )?,
    };

    if command.is_empty() {
        open_shell(&container_id, &config, &container_workspace_folder)
    } else {
        run_command(&container_id, &config, &container_workspace_folder, command)
    }
}

/// Create the image (building if needed), run the container, and execute the create/start lifecycle.
fn create_container(
    workspace: &Path,
    loaded: &LoadedConfig,
    config: &DevContainerConfig,
    ws_basename: &str,
    container_workspace_folder: &str,
    labels: &[(&str, String)],
) -> Result<String> {
    // Resolve the base image: build from a Dockerfile, or use a prebuilt image.
    let base_image = if config.is_dockerfile_build() {
        let tag = format!("devc-{}-{}", sanitize(ws_basename), short_hash(workspace));
        let dockerfile_rel = config.dockerfile().unwrap();
        let context = resolve_relative(&loaded.config_dir, &config.build_context());
        let dockerfile = resolve_relative(&context, &dockerfile_rel);
        let (build_args, target, cache_from, options) = match &config.build {
            Some(b) => (
                b.args.clone(),
                b.target.clone(),
                b.cache_from.as_vec(),
                b.options.clone(),
            ),
            None => Default::default(),
        };
        eprintln!("devc: building image {tag}");
        docker::build(
            &context,
            &dockerfile,
            &tag,
            &build_args,
            target.as_deref(),
            &cache_from,
            &options,
        )?;
        tag
    } else if let Some(image) = &config.image {
        image.clone()
    } else {
        bail!("devcontainer.json must specify either \"image\" or a Dockerfile build.");
    };

    // Apply Dev Container Features by building an image FROM the base image with each feature's
    // install.sh run in order.
    let mut image = base_image;
    let mut feature_runtime = crate::features::FeatureRuntime::default();
    if !config.features.is_empty() {
        let remote_user = config.remote_user.clone().unwrap_or_else(|| "root".to_string());
        let container_user = config
            .container_user
            .clone()
            .or_else(|| config.remote_user.clone())
            .unwrap_or_else(|| "root".to_string());
        let cache_key = format!("{}-{}", sanitize(ws_basename), short_hash(workspace));
        if let Some(prepared) = crate::features::prepare(
            &image,
            &config.features,
            &remote_user,
            &container_user,
            &cache_key,
        )? {
            let tag = format!("devc-features-{cache_key}");
            eprintln!("devc: building features image {tag}");
            docker::build(
                &prepared.context.context_dir,
                &prepared.context.dockerfile,
                &tag,
                &Default::default(),
                None,
                &[],
                &[],
            )?;
            image = tag;
            feature_runtime = prepared.runtime;
        }
    }

    let run_args = build_run_args(
        workspace,
        config,
        ws_basename,
        container_workspace_folder,
        labels,
        &image,
        &feature_runtime,
    )?;

    eprintln!("devc: creating container from {image}");
    let id = docker::run(&run_args)?;
    eprintln!("devc: started container {}", short(&id));

    // Full create/start lifecycle on a fresh container.
    run_lifecycle_in_container(
        &id,
        config,
        container_workspace_folder,
        &[
            config.on_create_command.as_ref(),
            config.update_content_command.as_ref(),
            config.post_create_command.as_ref(),
            config.post_start_command.as_ref(),
            config.post_attach_command.as_ref(),
        ],
    )?;

    Ok(id)
}

/// Assemble the argument list for `docker run` (everything after the `run` verb), mirroring the
/// order the reference CLI uses in `spawnDevContainer`.
fn build_run_args(
    workspace: &Path,
    config: &DevContainerConfig,
    ws_basename: &str,
    container_workspace_folder: &str,
    labels: &[(&str, String)],
    image: &str,
    feature_runtime: &crate::features::FeatureRuntime,
) -> Result<Vec<String>> {
    let mut args: Vec<String> = Vec::new();

    // Detached, keep stdio quiet — we attach later via `docker exec`.
    args.push("-d".to_string());

    // Forwarded ports (forwardPorts + appPort) → bind to loopback on the host.
    let mut ports = config.forward_ports.clone();
    if let Some(app) = &config.app_port {
        ports.extend(app.as_ports());
    }
    for port in &ports {
        args.push("-p".to_string());
        match port {
            config::Port::Number(n) => args.push(format!("127.0.0.1:{n}:{n}")),
            config::Port::Text(s) => args.push(s.clone()),
        }
    }

    // Workspace bind mount.
    let workspace_mount = config.workspace_mount.clone().unwrap_or_else(|| {
        let consistency = if cfg!(target_os = "linux") {
            String::new()
        } else {
            ",consistency=cached".to_string()
        };
        format!(
            "type=bind,source={},target={}{}",
            workspace.to_string_lossy(),
            container_workspace_folder,
            consistency
        )
    });
    let _ = ws_basename; // basename already folded into container_workspace_folder default.
    args.push("--mount".to_string());
    args.push(workspace_mount);

    // Additional mounts.
    for mount in &config.mounts {
        args.push("--mount".to_string());
        args.push(mount.to_mount_string());
    }

    // Labels.
    for (k, v) in labels {
        args.push("-l".to_string());
        args.push(format!("{k}={v}"));
    }
    args.push("-l".to_string());
    args.push(format!("{LABEL_MANAGED}=true"));

    // containerEnv.
    for (k, v) in &config.container_env {
        args.push("-e".to_string());
        args.push(format!("{k}={v}"));
    }

    // containerUser.
    if let Some(user) = &config.container_user {
        args.push("-u".to_string());
        args.push(user.clone());
    }

    // Passthrough runArgs.
    args.extend(config.run_args.iter().cloned());

    // Feature-style flags (from the config and requested by features).
    if config.init.unwrap_or(false) || feature_runtime.init {
        args.push("--init".to_string());
    }
    if config.privileged.unwrap_or(false) || feature_runtime.privileged {
        args.push("--privileged".to_string());
    }
    for cap in config.cap_add.iter().chain(&feature_runtime.cap_add) {
        args.push("--cap-add".to_string());
        args.push(cap.clone());
    }
    for opt in config.security_opt.iter().chain(&feature_runtime.security_opt) {
        args.push("--security-opt".to_string());
        args.push(opt.clone());
    }

    // Entrypoint / keep-alive command. Unless overrideCommand is false, replace the entrypoint with
    // a shell that runs any feature entrypoints and then keeps the container alive so we can exec in.
    let override_command = config.override_command.unwrap_or(true);
    if override_command {
        args.push("--entrypoint".to_string());
        args.push("/bin/sh".to_string());
        args.push(image.to_string());
        args.push("-c".to_string());
        args.push(keep_alive(&feature_runtime.entrypoints));
    } else {
        args.push(image.to_string());
    }

    Ok(args)
}

/// The keep-alive command used as the container's main process (matches the reference CLI). Feature
/// entrypoints run first; `trap`/`wait` let SIGTERM stop the container promptly.
fn keep_alive(entrypoints: &[String]) -> String {
    let mut s = String::from("echo Container started\ntrap \"exit 0\" 15\n");
    for ep in entrypoints {
        s.push_str(ep);
        s.push('\n');
    }
    s.push_str("exec \"$@\"\nwhile sleep 1 & wait $!; do :; done");
    s
}

/// Open an interactive login shell in the container, propagating its exit code.
fn open_shell(id: &str, config: &DevContainerConfig, cwd: &str) -> Result<i32> {
    // Prefer bash, fall back to sh; `exec` so signals/exit codes flow through cleanly.
    let cmd = vec![
        "/bin/sh".to_string(),
        "-c".to_string(),
        "exec \"$(command -v bash || command -v sh)\"".to_string(),
    ];
    exec_interactive(id, config, cwd, &cmd)
}

/// Run a user-supplied command inside the container (the `devc <command>...` form), propagating its
/// exit code. The command is run as `remoteUser` in the container workspace folder, with `remoteEnv`
/// applied — the same context as the interactive shell.
fn run_command(id: &str, config: &DevContainerConfig, cwd: &str, command: &[String]) -> Result<i32> {
    exec_interactive(id, config, cwd, command)
}

/// Exec `cmd` in the container as the remote user, attaching stdio, and return its exit code.
fn exec_interactive(
    id: &str,
    config: &DevContainerConfig,
    cwd: &str,
    cmd: &[String],
) -> Result<i32> {
    let user = effective_remote_user(config);
    let env = remote_env_pairs(config);
    // `docker exec` only accepts -t if our stdin is a real TTY. When devc is driven with piped input
    // (e.g. tests), we still want the command to run, reading any input from the pipe.
    let tty = std::io::stdin().is_terminal();
    let status = docker::exec(id, user.as_deref(), Some(cwd), &env, cmd, true, tty)?;
    Ok(exit_code(status))
}

/// Run a sequence of (optional) lifecycle commands inside the container, in order.
fn run_lifecycle_in_container(
    id: &str,
    config: &DevContainerConfig,
    cwd: &str,
    commands: &[Option<&LifecycleCommand>],
) -> Result<()> {
    let user = effective_remote_user(config);
    let env = remote_env_pairs(config);
    for cmd in commands.iter().flatten() {
        for argv in command_invocations(cmd) {
            let status = docker::exec(id, user.as_deref(), Some(cwd), &env, &argv, false, false)?;
            if !status.success() {
                bail!("lifecycle command failed: {}", argv.join(" "));
            }
        }
    }
    Ok(())
}

/// Run a lifecycle command on the host (used for initializeCommand).
fn run_host_command(cmd: &LifecycleCommand, cwd: &Path) -> Result<()> {
    for argv in command_invocations(cmd) {
        let mut command = to_std_command(&argv);
        command
            .current_dir(cwd)
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());
        let status = command
            .status()
            .with_context(|| format!("running {}", argv.join(" ")))?;
        if !status.success() {
            bail!("command failed: {}", argv.join(" "));
        }
    }
    Ok(())
}

/// Expand a lifecycle command value into one or more argv invocations. A shell string becomes
/// `sh -c <string>`; an argv array is used directly; a named map yields one invocation per entry.
fn command_invocations(cmd: &LifecycleCommand) -> Vec<Vec<String>> {
    match cmd {
        LifecycleCommand::Shell(s) => vec![shell_argv(s)],
        LifecycleCommand::Argv(a) => vec![a.clone()],
        LifecycleCommand::Named(map) => map
            .values()
            .map(|v| match v {
                StringOrArgv::Shell(s) => shell_argv(s),
                StringOrArgv::Argv(a) => a.clone(),
            })
            .collect(),
    }
}

fn shell_argv(s: &str) -> Vec<String> {
    vec!["/bin/sh".to_string(), "-c".to_string(), s.to_string()]
}

/// Build a std::process::Command from an argv, running via `sh -c` if the argv is a shell form.
fn to_std_command(argv: &[String]) -> Command {
    let mut c = Command::new(&argv[0]);
    c.args(&argv[1..]);
    c
}

/// The user to exec commands / the shell as: remoteUser, else containerUser.
fn effective_remote_user(config: &DevContainerConfig) -> Option<String> {
    config
        .remote_user
        .clone()
        .or_else(|| config.container_user.clone())
}

/// remoteEnv as key=value pairs, dropping entries explicitly set to null.
fn remote_env_pairs(config: &DevContainerConfig) -> Vec<(String, String)> {
    config
        .remote_env
        .iter()
        .filter_map(|(k, v)| v.as_ref().map(|v| (k.clone(), v.clone())))
        .collect()
}

/// Apply `${...}` substitution to the string-bearing fields of the config we act on.
fn substitute_config(mut config: DevContainerConfig, ctx: &SubstitutionContext) -> DevContainerConfig {
    let sub = |s: &str| config::substitute(s, ctx);

    config.workspace_folder = config.workspace_folder.as_deref().map(sub);
    config.workspace_mount = config.workspace_mount.as_deref().map(sub);
    config.remote_user = config.remote_user.as_deref().map(sub);
    config.container_user = config.container_user.as_deref().map(sub);

    config.container_env = config
        .container_env
        .iter()
        .map(|(k, v)| (k.clone(), sub(v)))
        .collect();
    config.remote_env = config
        .remote_env
        .iter()
        .map(|(k, v)| (k.clone(), v.as_deref().map(sub)))
        .collect();

    config.run_args = config.run_args.iter().map(|s| sub(s)).collect();
    config.mounts = config
        .mounts
        .into_iter()
        .map(|m| match m {
            config::Mount::Text(s) => config::Mount::Text(sub(&s)),
            config::Mount::Object(mut o) => {
                o.source = o.source.as_deref().map(sub);
                o.target = sub(&o.target);
                config::Mount::Object(o)
            }
        })
        .collect();

    config
}

// --- small path helpers ---

fn basename(p: &Path) -> String {
    p.file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| p.to_string_lossy().into_owned())
}

/// Resolve `rel` against `base`; absolute paths are returned as-is.
fn resolve_relative(base: &Path, rel: &str) -> PathBuf {
    let p = Path::new(rel);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        base.join(p)
    }
}

/// A short, stable hash of the workspace path for image tag uniqueness.
fn short_hash(p: &Path) -> String {
    let mut h = DefaultHasher::new();
    p.hash(&mut h);
    format!("{:08x}", h.finish() as u32)
}

fn short(id: &str) -> String {
    id.chars().take(12).collect()
}

/// Sanitize a string for use in a docker image tag (lowercase alnum/._-).
fn sanitize(s: &str) -> String {
    let cleaned: String = s
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let trimmed = cleaned.trim_matches(|c| c == '-' || c == '.' || c == '_');
    if trimmed.is_empty() {
        "workspace".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Map an ExitStatus to a process exit code, translating signals to 128+signal on Unix.
fn exit_code(status: std::process::ExitStatus) -> i32 {
    if let Some(code) = status.code() {
        return code;
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(sig) = status.signal() {
            return 128 + sig;
        }
    }
    1
}
