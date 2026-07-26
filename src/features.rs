//! Dev Container Features support.
//!
//! Features are distributed as OCI artifacts (e.g. `ghcr.io/devcontainers/features/java:1`). For each
//! feature referenced by the config we:
//!   1. fetch its OCI manifest (with anonymous bearer auth), find the tar layer, download + extract it
//!      (cached by digest under the user cache dir),
//!   2. parse its `devcontainer-feature.json` (option definitions, `containerEnv`, `installsAfter`),
//!   3. merge the user-provided option values with the defaults, and
//!   4. emit an extended Dockerfile that `COPY`s each feature and runs its `install.sh` with the option
//!      values exported as env vars (following the reference CLI's `getSafeId` naming), plus `ENV`
//!      lines for each feature's `containerEnv`.
//!
//! The extended image is then built `FROM` the base image, so `devc` shells into a container with the
//! features installed.

use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;
use serde_json::{Map, Value};

const MANIFEST_MEDIA_TYPE: &str = "application/vnd.oci.image.manifest.v1+json";
const LAYER_MEDIA_TYPE: &str = "application/vnd.devcontainers.layer.v1+tar";

/// A generated build context for the features-extended image.
pub struct BuiltContext {
    /// Directory to pass to `docker build` as context.
    pub context_dir: PathBuf,
    /// Absolute path of the generated Dockerfile.
    pub dockerfile: PathBuf,
}

/// A feature resolved from the registry, with its metadata and merged option values.
struct ResolvedFeature {
    /// The full reference without tag, e.g. `ghcr.io/devcontainers/features/java`.
    ref_no_tag: String,
    /// Local directory (in the build context) the feature was copied into.
    folder: String,
    /// Absolute path to the extracted feature source.
    src_dir: PathBuf,
    metadata: FeatureMetadata,
    /// Option values as `(SAFE_ID, value)` pairs, ready to export before `install.sh`.
    option_env: Vec<(String, String)>,
}

/// The subset of `devcontainer-feature.json` we consume.
#[derive(Debug, Deserialize, Default)]
struct FeatureMetadata {
    #[allow(dead_code)]
    id: Option<String>,
    #[allow(dead_code)]
    version: Option<String>,
    #[serde(default)]
    options: Map<String, Value>,
    #[serde(default, rename = "containerEnv")]
    container_env: Map<String, Value>,
    #[serde(default, rename = "installsAfter")]
    installs_after: Vec<String>,
    entrypoint: Option<String>,
    init: Option<bool>,
    privileged: Option<bool>,
    #[serde(default, rename = "capAdd")]
    cap_add: Vec<String>,
    #[serde(default, rename = "securityOpt")]
    security_opt: Vec<String>,
}

/// Runtime flags a feature can request on the container (surfaced so `docker run` can apply them).
#[derive(Debug, Default)]
pub struct FeatureRuntime {
    pub init: bool,
    pub privileged: bool,
    pub cap_add: Vec<String>,
    pub security_opt: Vec<String>,
    pub entrypoints: Vec<String>,
}

/// Result of preparing features: the build context plus any container runtime requirements.
pub struct Prepared {
    pub context: BuiltContext,
    pub runtime: FeatureRuntime,
}

/// Prepare an extended image build for the given base image and feature set. Returns `Ok(None)` when
/// there are no (enabled) features. `remote_user`/`container_user` feed the feature install env.
pub fn prepare(
    base_image: &str,
    features: &Map<String, Value>,
    remote_user: &str,
    container_user: &str,
    cache_key: &str,
) -> Result<Option<Prepared>> {
    // Collect enabled features in config order (a `false` value disables a feature).
    let enabled: Vec<(&String, &Value)> = features
        .iter()
        .filter(|(_, v)| !matches!(v, Value::Bool(false)))
        .collect();
    if enabled.is_empty() {
        return Ok(None);
    }

    let agent = build_agent()?;

    let mut resolved: Vec<ResolvedFeature> = Vec::new();
    for (idx, (feature_ref, options)) in enabled.iter().enumerate() {
        eprintln!("devc: fetching feature {feature_ref}");
        let (registry, repository, tag) = parse_ref(feature_ref);
        let ref_no_tag = format!("{registry}/{repository}");

        let src_dir = fetch_feature(&agent, &registry, &repository, &tag)
            .with_context(|| format!("fetching feature {feature_ref}"))?;

        let metadata = read_metadata(&src_dir)
            .with_context(|| format!("reading metadata for {feature_ref}"))?;

        let option_env = merge_options(&metadata.options, options);

        resolved.push(ResolvedFeature {
            ref_no_tag,
            folder: format!("feature{idx}"),
            src_dir,
            metadata,
            option_env,
        });
    }

    // Order by installsAfter (stable to config order).
    let order = install_order(&resolved);
    let ordered: Vec<&ResolvedFeature> = order.iter().map(|&i| &resolved[i]).collect();

    // Build the context directory: copy each feature in and write the Dockerfile.
    let context_dir = build_context_dir(cache_key)?;
    let features_root = context_dir.join("dc-features");
    std::fs::create_dir_all(&features_root)
        .with_context(|| format!("creating {}", features_root.display()))?;
    for f in &ordered {
        let dest = features_root.join(&f.folder);
        copy_dir_all(&f.src_dir, &dest)
            .with_context(|| format!("copying feature into {}", dest.display()))?;
    }

    let dockerfile_text =
        render_dockerfile(base_image, &ordered, remote_user, container_user);
    let dockerfile = context_dir.join("Dockerfile.devc-features");
    std::fs::write(&dockerfile, dockerfile_text)
        .with_context(|| format!("writing {}", dockerfile.display()))?;

    // Aggregate runtime requirements from all features.
    let mut runtime = FeatureRuntime::default();
    for f in &ordered {
        let m = &f.metadata;
        runtime.init |= m.init.unwrap_or(false);
        runtime.privileged |= m.privileged.unwrap_or(false);
        runtime.cap_add.extend(m.cap_add.iter().cloned());
        runtime.security_opt.extend(m.security_opt.iter().cloned());
        if let Some(ep) = &m.entrypoint {
            runtime.entrypoints.push(ep.clone());
        }
    }

    Ok(Some(Prepared {
        context: BuiltContext {
            context_dir,
            dockerfile,
        },
        runtime,
    }))
}

// --- OCI fetch ---

fn build_agent() -> Result<ureq::Agent> {
    // Don't treat 4xx/5xx as transport errors: we need to inspect 401 (auth challenge) ourselves.
    let config = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .build();
    Ok(config.into())
}

struct HttpResponse {
    status: u16,
    www_authenticate: Option<String>,
    body: Vec<u8>,
}

fn http_get(
    agent: &ureq::Agent,
    url: &str,
    accept: Option<&str>,
    token: Option<&str>,
) -> Result<HttpResponse> {
    let mut req = agent.get(url);
    if let Some(a) = accept {
        req = req.header("Accept", a);
    }
    if let Some(t) = token {
        req = req.header("Authorization", &format!("Bearer {t}"));
    }
    let resp = req.call().with_context(|| format!("GET {url}"))?;
    let status = resp.status().as_u16();
    let www_authenticate = resp
        .headers()
        .get("www-authenticate")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    let mut body = Vec::new();
    resp.into_body()
        .into_reader()
        .read_to_end(&mut body)
        .with_context(|| format!("reading response body from {url}"))?;
    Ok(HttpResponse {
        status,
        www_authenticate,
        body,
    })
}

/// GET a URL, transparently performing the OCI anonymous bearer-token dance on a 401.
fn http_get_authed(
    agent: &ureq::Agent,
    url: &str,
    accept: Option<&str>,
    repository: &str,
) -> Result<HttpResponse> {
    let first = http_get(agent, url, accept, None)?;
    if first.status != 401 {
        return Ok(first);
    }
    let challenge = first
        .www_authenticate
        .as_deref()
        .ok_or_else(|| anyhow!("registry returned 401 without a WWW-Authenticate header"))?;
    let token = fetch_token(agent, challenge, repository)?;
    http_get(agent, url, accept, Some(&token))
}

/// Parse a `Bearer realm="...",service="...",scope="..."` challenge, request a token, return it.
fn fetch_token(agent: &ureq::Agent, challenge: &str, repository: &str) -> Result<String> {
    let params = parse_www_authenticate(challenge);
    let realm = params
        .iter()
        .find(|(k, _)| k == "realm")
        .map(|(_, v)| v.clone())
        .ok_or_else(|| anyhow!("auth challenge missing realm: {challenge:?}"))?;
    let service = params.iter().find(|(k, _)| k == "service").map(|(_, v)| v.clone());
    let scope = params
        .iter()
        .find(|(k, _)| k == "scope")
        .map(|(_, v)| v.clone())
        .unwrap_or_else(|| format!("repository:{repository}:pull"));

    let mut url = format!("{realm}?scope={}", urlencode(&scope));
    if let Some(service) = service {
        url.push_str(&format!("&service={}", urlencode(&service)));
    }

    let resp = http_get(agent, &url, Some("application/json"), None)?;
    if resp.status / 100 != 2 {
        bail!("token request failed with HTTP {}", resp.status);
    }
    let json: Value = serde_json::from_slice(&resp.body).context("parsing token response")?;
    json.get("token")
        .or_else(|| json.get("access_token"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow!("token response contained no token"))
}

fn parse_www_authenticate(header: &str) -> Vec<(String, String)> {
    // Strip a leading scheme (e.g. "Bearer ") then split comma-separated key="value" pairs.
    let rest = header.strip_prefix("Bearer ").unwrap_or(header);
    let mut out = Vec::new();
    for part in rest.split(',') {
        if let Some((k, v)) = part.split_once('=') {
            let v = v.trim().trim_matches('"');
            out.push((k.trim().to_string(), v.to_string()));
        }
    }
    out
}

/// Minimal percent-encoding for query values (encodes reserved chars we care about).
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Fetch and extract a feature's tar layer, caching by digest. Returns the extracted source dir.
fn fetch_feature(
    agent: &ureq::Agent,
    registry: &str,
    repository: &str,
    tag: &str,
) -> Result<PathBuf> {
    let manifest_url = format!("https://{registry}/v2/{repository}/manifests/{tag}");
    let resp = http_get_authed(agent, &manifest_url, Some(MANIFEST_MEDIA_TYPE), repository)?;
    if resp.status / 100 != 2 {
        bail!("fetching manifest {manifest_url} failed with HTTP {}", resp.status);
    }
    let manifest: Value = serde_json::from_slice(&resp.body).context("parsing OCI manifest")?;

    let digest = manifest
        .get("layers")
        .and_then(|l| l.as_array())
        .and_then(|layers| {
            layers
                .iter()
                .find(|layer| {
                    layer.get("mediaType").and_then(|m| m.as_str()) == Some(LAYER_MEDIA_TYPE)
                })
                .or_else(|| layers.first())
        })
        .and_then(|layer| layer.get("digest").and_then(|d| d.as_str()))
        .ok_or_else(|| anyhow!("no tar layer found in manifest for {repository}:{tag}"))?
        .to_string();

    let cache_dir = feature_cache_dir(&digest)?;
    let marker = cache_dir.join("devcontainer-feature.json");
    if marker.is_file() {
        return Ok(cache_dir); // already extracted
    }

    let blob_url = format!("https://{registry}/v2/{repository}/blobs/{digest}");
    let blob = http_get_authed(agent, &blob_url, None, repository)?;
    if blob.status / 100 != 2 {
        bail!("downloading blob {digest} failed with HTTP {}", blob.status);
    }

    // Extract into a temp dir, then atomically rename into the cache location.
    let tmp = cache_dir.with_extension("tmp");
    if tmp.exists() {
        let _ = std::fs::remove_dir_all(&tmp);
    }
    std::fs::create_dir_all(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
    let mut archive = tar::Archive::new(std::io::Cursor::new(&blob.body));
    archive
        .unpack(&tmp)
        .with_context(|| format!("extracting feature tar for {repository}:{tag}"))?;

    if cache_dir.exists() {
        let _ = std::fs::remove_dir_all(&cache_dir);
    }
    std::fs::rename(&tmp, &cache_dir)
        .with_context(|| format!("moving extracted feature into {}", cache_dir.display()))?;
    Ok(cache_dir)
}

fn read_metadata(dir: &Path) -> Result<FeatureMetadata> {
    let path = dir.join("devcontainer-feature.json");
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("reading {}", path.display()))?;
    let metadata: FeatureMetadata =
        json5::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    Ok(metadata)
}

// --- option merge & install order ---

/// Merge a feature's option definitions (with their defaults) against the user-provided value, and
/// return `(SAFE_ID, value)` env pairs to export before `install.sh` runs.
fn merge_options(defs: &Map<String, Value>, user: &Value) -> Vec<(String, String)> {
    // Start from defaults, preserving definition order.
    let mut values: Map<String, Value> = Map::new();
    for (k, def) in defs {
        if let Some(default) = def.get("default") {
            values.insert(k.clone(), default.clone());
        }
    }

    // Apply user overrides.
    match user {
        Value::Object(obj) => {
            for (k, v) in obj {
                values.insert(k.clone(), v.clone());
            }
        }
        // A scalar sets the feature's main option. The reference uses the option literally named
        // "version" when present, else the sole option. `true` just means "use defaults".
        Value::String(_) | Value::Number(_) => {
            if let Some(main) = main_option(defs) {
                values.insert(main, user.clone());
            }
        }
        _ => {}
    }

    values
        .iter()
        .map(|(k, v)| (safe_id(k), value_to_string(v)))
        .collect()
}

/// The name of the "main" option a scalar feature value maps to: `version` if defined, else the only
/// option if there is exactly one.
fn main_option(defs: &Map<String, Value>) -> Option<String> {
    if defs.contains_key("version") {
        return Some("version".to_string());
    }
    if defs.len() == 1 {
        return defs.keys().next().cloned();
    }
    None
}

fn value_to_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// Mirror the reference CLI's `getSafeId`: non-word chars → `_`, a leading digit/underscore run → a
/// single `_`, then uppercase.
fn safe_id(name: &str) -> String {
    let replaced: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' { c } else { '_' })
        .collect();
    let trimmed = replaced.trim_start_matches(|c: char| c.is_ascii_digit() || c == '_');
    let normalized = if trimmed.len() != replaced.len() {
        format!("_{trimmed}")
    } else {
        replaced
    };
    normalized.to_uppercase()
}

/// Topologically order features by `installsAfter`, stable to config order and tolerant of cycles.
fn install_order(features: &[ResolvedFeature]) -> Vec<usize> {
    let n = features.len();
    // prereqs[i] = indices that must be installed before feature i.
    let mut prereqs: Vec<Vec<usize>> = vec![Vec::new(); n];
    for (i, f) in features.iter().enumerate() {
        for after in &f.metadata.installs_after {
            for (j, other) in features.iter().enumerate() {
                if i != j && ref_matches(&other.ref_no_tag, &other.metadata, after) {
                    prereqs[i].push(j);
                }
            }
        }
    }

    let mut remaining: Vec<usize> = (0..n).collect();
    let mut output: Vec<usize> = Vec::with_capacity(n);
    while !remaining.is_empty() {
        // Pick the lowest-index node whose prerequisites are all already emitted.
        let pos = remaining
            .iter()
            .position(|&i| prereqs[i].iter().all(|p| output.contains(p)));
        // On a cycle (no satisfiable node), fall back to config order to make progress.
        let idx = pos.unwrap_or(0);
        output.push(remaining.remove(idx));
    }
    output
}

/// Whether an `installsAfter` entry refers to the given feature.
fn ref_matches(ref_no_tag: &str, metadata: &FeatureMetadata, entry: &str) -> bool {
    let entry = entry.split(':').next().unwrap_or(entry); // drop any tag
    ref_no_tag == entry
        || ref_no_tag.ends_with(&format!("/{entry}"))
        || metadata.id.as_deref() == Some(entry)
}

// --- Dockerfile generation ---

fn render_dockerfile(
    base_image: &str,
    features: &[&ResolvedFeature],
    remote_user: &str,
    container_user: &str,
) -> String {
    let mut out = String::new();
    out.push_str(&format!("FROM {base_image}\n\n"));
    out.push_str("USER root\n\n");

    for f in features {
        // Emit this feature's containerEnv first so its own install.sh and later features see it.
        for (k, v) in &f.metadata.container_env {
            out.push_str(&format!("ENV {}={}\n", k, env_value(&value_to_string(v))));
        }

        out.push_str(&format!(
            "COPY dc-features/{folder} /tmp/dc-features/{folder}\n",
            folder = f.folder
        ));

        let mut run = String::new();
        run.push_str(&format!("cd /tmp/dc-features/{} \\\n", f.folder));
        run.push_str("    && chmod +x ./install.sh \\\n");
        run.push_str(&format!(
            "    && export _REMOTE_USER={ru} _CONTAINER_USER={cu} \\\n",
            ru = sh_quote(remote_user),
            cu = sh_quote(container_user),
        ));
        run.push_str(&format!(
            "    && export _REMOTE_USER_HOME=\"$(getent passwd {ru} | cut -d: -f6)\" \\\n",
            ru = sh_quote(remote_user),
        ));
        run.push_str(&format!(
            "    && export _CONTAINER_USER_HOME=\"$(getent passwd {cu} | cut -d: -f6)\" \\\n",
            cu = sh_quote(container_user),
        ));
        for (k, v) in &f.option_env {
            run.push_str(&format!("    && export {k}={} \\\n", sh_quote(v)));
        }
        run.push_str("    && ./install.sh \\\n");
        run.push_str(&format!("    && rm -rf /tmp/dc-features/{}\n", f.folder));

        out.push_str(&format!("RUN {run}\n"));
    }

    out
}

/// Quote a value for a Dockerfile `ENV KEY=VALUE`, wrapping in double quotes so `${PATH}`-style refs
/// still expand against the build environment.
fn env_value(v: &str) -> String {
    format!("\"{}\"", v.replace('"', "\\\""))
}

/// Single-quote a value for use inside the generated `RUN` shell script.
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

// --- filesystem helpers ---

/// A fresh, empty build-context directory for this workspace's features image.
fn build_context_dir(cache_key: &str) -> Result<PathBuf> {
    let dir = std::env::temp_dir().join(format!("devc-features-{cache_key}"));
    if dir.exists() {
        std::fs::remove_dir_all(&dir)
            .with_context(|| format!("clearing {}", dir.display()))?;
    }
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    Ok(dir)
}

/// Per-digest cache directory for an extracted feature.
fn feature_cache_dir(digest: &str) -> Result<PathBuf> {
    let base = if let Ok(x) = std::env::var("XDG_CACHE_HOME") {
        PathBuf::from(x)
    } else if let Ok(home) = std::env::var("HOME") {
        PathBuf::from(home).join(".cache")
    } else if let Ok(profile) = std::env::var("USERPROFILE") {
        PathBuf::from(profile).join(".cache")
    } else {
        std::env::temp_dir()
    };
    let dir = base.join("devc").join("features").join(digest.replace(':', "_"));
    if let Some(parent) = dir.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating cache dir {}", parent.display()))?;
    }
    Ok(dir)
}

fn copy_dir_all(src: &Path, dst: &Path) -> Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if file_type.is_dir() {
            copy_dir_all(&from, &to)?;
        } else {
            std::fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

// --- reference parsing ---

/// Parse a feature ref into `(registry, repository, tag)`.
fn parse_ref(reference: &str) -> (String, String, String) {
    // Separate an optional tag: the last ':' that is not part of the registry host:port and is after
    // the final '/'.
    let (name, tag) = match reference.rsplit_once(':') {
        Some((n, t)) if !t.contains('/') => (n, t),
        _ => (reference, "latest"),
    };

    let (registry, repository) = match name.split_once('/') {
        Some((first, rest))
            if first.contains('.') || first.contains(':') || first == "localhost" =>
        {
            (first.to_string(), rest.to_string())
        }
        _ => ("registry-1.docker.io".to_string(), name.to_string()),
    };

    (registry, repository, tag.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_id_matches_reference() {
        assert_eq!(safe_id("version"), "VERSION");
        assert_eq!(safe_id("jdkDistro"), "JDKDISTRO");
        assert_eq!(safe_id("installGradle"), "INSTALLGRADLE");
        assert_eq!(safe_id("install-gradle"), "INSTALL_GRADLE");
        assert_eq!(safe_id("2foo"), "_FOO");
        assert_eq!(safe_id("_foo"), "_FOO");
    }

    #[test]
    fn parse_ref_splits_registry_and_tag() {
        let (r, repo, tag) = parse_ref("ghcr.io/devcontainers/features/java:1");
        assert_eq!(r, "ghcr.io");
        assert_eq!(repo, "devcontainers/features/java");
        assert_eq!(tag, "1");

        let (_, _, tag) = parse_ref("ghcr.io/devcontainers/features/github-cli");
        assert_eq!(tag, "latest");
    }

    #[test]
    fn merge_options_applies_defaults_and_overrides() {
        let defs: Map<String, Value> = serde_json::from_str(
            r#"{
                "version": {"default": "latest"},
                "jdkDistro": {"default": "ms"},
                "installGradle": {"default": false}
            }"#,
        )
        .unwrap();
        let user: Value = serde_json::from_str(
            r#"{"version": "25.0.3-tem", "jdkDistro": "tem", "installGradle": false}"#,
        )
        .unwrap();
        let env = merge_options(&defs, &user);
        assert!(env.contains(&("VERSION".to_string(), "25.0.3-tem".to_string())));
        assert!(env.contains(&("JDKDISTRO".to_string(), "tem".to_string())));
        assert!(env.contains(&("INSTALLGRADLE".to_string(), "false".to_string())));
    }

    #[test]
    fn scalar_value_sets_main_option() {
        let defs: Map<String, Value> =
            serde_json::from_str(r#"{"version": {"default": "latest"}}"#).unwrap();
        let env = merge_options(&defs, &Value::String("1".to_string()));
        assert_eq!(env, vec![("VERSION".to_string(), "1".to_string())]);
    }
}
