//! Locating, parsing, and normalizing a `devcontainer.json` for a workspace.
//!
//! Only the single-container subset of the Dev Containers spec is supported (an `image` or a
//! Dockerfile build). docker-compose configs are rejected. Dev Container Features are supported and
//! installed into an extended image (see `features.rs`).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;

/// A lifecycle command, which the spec allows to be a shell string, an argv array, or a map of
/// named commands (which the reference CLI runs in parallel — we run them sequentially).
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum Command {
    Shell(String),
    Argv(Vec<String>),
    Named(BTreeMap<String, StringOrArgv>),
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum StringOrArgv {
    Shell(String),
    Argv(Vec<String>),
}

/// A port to forward, either a bare number or a `host:container` / `ip:host:container` string.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum Port {
    Number(u32),
    Text(String),
}

/// A mount entry: either a raw `--mount` string or a structured object.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum Mount {
    Text(String),
    Object(MountObject),
}

#[derive(Debug, Clone, Deserialize)]
pub struct MountObject {
    #[serde(rename = "type")]
    pub kind: Option<String>,
    pub source: Option<String>,
    pub target: String,
}

impl Mount {
    /// Render to the value passed after `--mount` on the docker command line.
    pub fn to_mount_string(&self) -> String {
        match self {
            Mount::Text(s) => s.clone(),
            Mount::Object(o) => {
                let mut parts = Vec::new();
                parts.push(format!("type={}", o.kind.as_deref().unwrap_or("bind")));
                if let Some(src) = &o.source {
                    parts.push(format!("source={src}"));
                }
                parts.push(format!("target={}", o.target));
                parts.join(",")
            }
        }
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct BuildConfig {
    pub dockerfile: Option<String>,
    pub context: Option<String>,
    #[serde(default)]
    pub args: BTreeMap<String, String>,
    pub target: Option<String>,
    #[serde(default, rename = "cacheFrom")]
    pub cache_from: CacheFrom,
    #[serde(default)]
    pub options: Vec<String>,
}

/// `cacheFrom` may be a single string or a list.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum CacheFrom {
    One(String),
    Many(Vec<String>),
}

impl Default for CacheFrom {
    fn default() -> Self {
        CacheFrom::Many(Vec::new())
    }
}

impl CacheFrom {
    pub fn as_vec(&self) -> Vec<String> {
        match self {
            CacheFrom::One(s) => vec![s.clone()],
            CacheFrom::Many(v) => v.clone(),
        }
    }
}

/// The parsed `devcontainer.json`. Unknown keys are ignored so that configs using features we don't
/// model still parse.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct DevContainerConfig {
    #[allow(dead_code)] // parsed for completeness; not currently used by devc
    pub name: Option<String>,

    // Image vs. build.
    pub image: Option<String>,
    pub build: Option<BuildConfig>,
    #[serde(rename = "dockerFile")]
    pub docker_file: Option<String>, // legacy top-level Dockerfile path
    pub context: Option<String>, // legacy top-level build context

    // Workspace.
    #[serde(rename = "workspaceFolder")]
    pub workspace_folder: Option<String>,
    #[serde(rename = "workspaceMount")]
    pub workspace_mount: Option<String>,

    // Users.
    #[serde(rename = "remoteUser")]
    pub remote_user: Option<String>,
    #[serde(rename = "containerUser")]
    pub container_user: Option<String>,

    // Environment.
    #[serde(default, rename = "containerEnv")]
    pub container_env: BTreeMap<String, String>,
    #[serde(default, rename = "remoteEnv")]
    pub remote_env: BTreeMap<String, Option<String>>,

    // Runtime knobs.
    #[serde(default)]
    pub mounts: Vec<Mount>,
    #[serde(default, rename = "runArgs")]
    pub run_args: Vec<String>,
    #[serde(default, rename = "forwardPorts")]
    pub forward_ports: Vec<Port>,
    #[serde(rename = "appPort")]
    pub app_port: Option<AppPort>,
    #[serde(rename = "overrideCommand")]
    pub override_command: Option<bool>,
    pub init: Option<bool>,
    pub privileged: Option<bool>,
    #[serde(default, rename = "capAdd")]
    pub cap_add: Vec<String>,
    #[serde(default, rename = "securityOpt")]
    pub security_opt: Vec<String>,

    // Lifecycle commands.
    #[serde(rename = "initializeCommand")]
    pub initialize_command: Option<Command>,
    #[serde(rename = "onCreateCommand")]
    pub on_create_command: Option<Command>,
    #[serde(rename = "updateContentCommand")]
    pub update_content_command: Option<Command>,
    #[serde(rename = "postCreateCommand")]
    pub post_create_command: Option<Command>,
    #[serde(rename = "postStartCommand")]
    pub post_start_command: Option<Command>,
    #[serde(rename = "postAttachCommand")]
    pub post_attach_command: Option<Command>,

    // Dev Container Features, keyed by feature ref (order preserved via serde_json preserve_order).
    // The value is the feature's options: `true`/`{}` for defaults, a scalar for the main option,
    // or an object of option overrides. `false` disables the feature.
    #[serde(default)]
    pub features: serde_json::Map<String, serde_json::Value>,

    // Unsupported — detected to produce a clear message.
    #[serde(rename = "dockerComposeFile")]
    pub docker_compose_file: Option<serde_json::Value>,
}

/// `appPort` may be a number, a string, or a list of either.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum AppPort {
    One(Port),
    Many(Vec<Port>),
}

impl AppPort {
    pub fn as_ports(&self) -> Vec<Port> {
        match self {
            AppPort::One(p) => vec![p.clone()],
            AppPort::Many(v) => v.clone(),
        }
    }
}

/// A parsed config together with the paths it was resolved from.
pub struct LoadedConfig {
    pub config: DevContainerConfig,
    /// Absolute path to the `devcontainer.json` file.
    pub config_path: PathBuf,
    /// Directory containing the config file (the docker build context base for relative paths).
    pub config_dir: PathBuf,
}

/// Find the `devcontainer.json` for `workspace`, mirroring the reference CLI's well-known paths:
/// `.devcontainer/devcontainer.json` then `.devcontainer.json`.
pub fn find_config_path(workspace: &Path) -> Option<PathBuf> {
    let candidates = [
        workspace.join(".devcontainer").join("devcontainer.json"),
        workspace.join(".devcontainer.json"),
    ];
    candidates.into_iter().find(|p| p.is_file())
}

/// Load and parse the config for a workspace, returning an error when none exists or the config uses
/// an unsupported feature.
pub fn load(workspace: &Path) -> Result<LoadedConfig> {
    let config_path = find_config_path(workspace).ok_or_else(|| {
        anyhow!(
            "No dev container configuration found in {} \
             (looked for .devcontainer/devcontainer.json and .devcontainer.json).",
            workspace.display()
        )
    })?;

    let text = std::fs::read_to_string(&config_path)
        .with_context(|| format!("reading {}", config_path.display()))?;

    // devcontainer.json is JSONC (comments, trailing commas). json5 is a superset that accepts both.
    let config: DevContainerConfig = json5::from_str(&text)
        .with_context(|| format!("parsing {}", config_path.display()))?;

    if config.docker_compose_file.is_some() {
        bail!(
            "{}: docker-compose dev containers are not supported by devc.",
            config_path.display()
        );
    }

    let config_dir = config_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| workspace.to_path_buf());

    Ok(LoadedConfig {
        config,
        config_path,
        config_dir,
    })
}

impl DevContainerConfig {
    /// Whether the container image is produced from a Dockerfile build.
    pub fn is_dockerfile_build(&self) -> bool {
        self.docker_file.is_some()
            || self
                .build
                .as_ref()
                .map(|b| b.dockerfile.is_some())
                .unwrap_or(false)
    }

    /// The Dockerfile path relative to the build context, if this is a build config.
    pub fn dockerfile(&self) -> Option<String> {
        self.docker_file
            .clone()
            .or_else(|| self.build.as_ref().and_then(|b| b.dockerfile.clone()))
    }

    /// The build context directory relative to the config folder, defaulting to ".".
    pub fn build_context(&self) -> String {
        self.build
            .as_ref()
            .and_then(|b| b.context.clone())
            .or_else(|| self.context.clone())
            .unwrap_or_else(|| ".".to_string())
    }
}

/// Substitute `${...}` variables in a string. Supports the variables devc understands; unknown
/// variables are left untouched.
pub fn substitute(input: &str, vars: &SubstitutionContext) -> String {
    let mut out = String::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'$' && i + 1 < bytes.len() && bytes[i + 1] == b'{' {
            if let Some(end) = input[i + 2..].find('}') {
                let expr = &input[i + 2..i + 2 + end];
                out.push_str(&vars.resolve(expr).unwrap_or_else(|| format!("${{{expr}}}")));
                i = i + 2 + end + 1;
                continue;
            }
        }
        // Copy one UTF-8 char.
        let ch_len = utf8_char_len(bytes[i]);
        out.push_str(&input[i..i + ch_len]);
        i += ch_len;
    }
    out
}

fn utf8_char_len(b: u8) -> usize {
    match b {
        0x00..=0x7F => 1,
        0xC0..=0xDF => 2,
        0xE0..=0xEF => 3,
        _ => 4,
    }
}

/// Values available to `${...}` substitution.
pub struct SubstitutionContext {
    pub local_workspace_folder: String,
    pub local_workspace_folder_basename: String,
    pub container_workspace_folder: String,
    pub container_workspace_folder_basename: String,
}

impl SubstitutionContext {
    fn resolve(&self, expr: &str) -> Option<String> {
        match expr {
            "localWorkspaceFolder" => Some(self.local_workspace_folder.clone()),
            "localWorkspaceFolderBasename" => Some(self.local_workspace_folder_basename.clone()),
            "containerWorkspaceFolder" => Some(self.container_workspace_folder.clone()),
            "containerWorkspaceFolderBasename" => {
                Some(self.container_workspace_folder_basename.clone())
            }
            _ => {
                if let Some(rest) = expr.strip_prefix("localEnv:") {
                    Some(resolve_env(rest))
                } else {
                    expr.strip_prefix("containerEnv:").map(resolve_env)
                }
            }
        }
    }
}

/// Resolve `VAR` or `VAR:default` against the host environment.
fn resolve_env(rest: &str) -> String {
    let (name, default) = match rest.split_once(':') {
        Some((n, d)) => (n, d),
        None => (rest, ""),
    };
    std::env::var(name).unwrap_or_else(|_| default.to_string())
}
