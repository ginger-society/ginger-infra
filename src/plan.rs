use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use walkdir::WalkDir;

// ── Snapshot types ────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct SnapshotService {
    pub identifier: String,
    pub version: String,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct SnapshotPackage {
    pub identifier: String,
    pub version: String,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct SnapshotDatabase {
    pub name: String,
    pub version: String,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct Snapshot {
    pub services: Vec<SnapshotService>,
    pub packages: Vec<SnapshotPackage>,
    pub databases: Vec<SnapshotDatabase>,
}

// ── .gitignore check ──────────────────────────────────────────────────────────

fn check_gitignore() -> anyhow::Result<()> {
    let gitignore = fs::read_to_string(".gitignore").unwrap_or_default();
    let mut missing: Vec<&str> = Vec::new();

    if !gitignore.lines().any(|l| l.trim() == "values.json" || l.trim() == "/values.json") {
        missing.push("values.json");
    }
    if !gitignore.lines().any(|l| l.trim() == "values.memory.json" || l.trim() == "/values.memory.json") {
        missing.push("values.memory.json");
    }
    if !gitignore.lines().any(|l| l.trim() == "build/" || l.trim() == "/build/") {
        missing.push("build/");
    }

    if !missing.is_empty() {
        anyhow::bail!(
            "The following entries are missing from .gitignore:\n{}\n\
             Add them before running plan.",
            missing.iter().map(|e| format!("  - {}", e)).collect::<Vec<_>>().join("\n")
        );
    }

    println!("✓ .gitignore verified");
    Ok(())
}

// ── Snapshot validation ───────────────────────────────────────────────────────

fn validate_snapshot(snapshot: &Snapshot) -> anyhow::Result<()> {
    let mut errors: Vec<String> = Vec::new();
    let mut seen_services: HashSet<String> = HashSet::new();
    let mut seen_packages: HashSet<String> = HashSet::new();
    let mut seen_databases: HashSet<String> = HashSet::new();

    for s in &snapshot.services {
        if s.version.trim().is_empty() {
            errors.push(format!("service '{}' has an empty version", s.identifier));
        }
        if !seen_services.insert(s.identifier.clone()) {
            errors.push(format!("service '{}' is duplicated in snapshot.json", s.identifier));
        }
    }
    for p in &snapshot.packages {
        if p.version.trim().is_empty() {
            errors.push(format!("package '{}' has an empty version", p.identifier));
        }
        if !seen_packages.insert(p.identifier.clone()) {
            errors.push(format!("package '{}' is duplicated in snapshot.json", p.identifier));
        }
    }
    for d in &snapshot.databases {
        if d.version.trim().is_empty() {
            errors.push(format!("database '{}' has an empty version", d.name));
        }
        if !seen_databases.insert(d.name.clone()) {
            errors.push(format!("database '{}' is duplicated in snapshot.json", d.name));
        }
    }

    if !errors.is_empty() {
        eprintln!("\n✗ snapshot.json validation errors:");
        for e in &errors { eprintln!("    - {}", e); }
        anyhow::bail!("Fix snapshot.json errors before running plan.");
    }

    println!("✓ snapshot.json validated");
    Ok(())
}

// ── Value resolution ──────────────────────────────────────────────────────────

/// Resolved value — either a concrete string, a vault placeholder, or a
/// deferred shell command that will be run at render time with the correct
/// .envrc context for the file being rendered.
#[derive(Debug, Clone)]
pub enum ResolvedValue {
    /// A concrete value ready to be substituted into templates
    Concrete(String),
    /// A vault key — substitution deferred to deploy time
    Vault(String),
    /// A shell command — deferred to render time so the correct .envrc
    /// (e.g. mcs/.envrc with KUBECONFIG) is applied when it runs
    Shell(String),
}

impl ResolvedValue {
    pub fn as_template_str(&self) -> String {
        match self {
            ResolvedValue::Concrete(v) => v.clone(),
            ResolvedValue::Vault(key)  => format!("vault({})", key),
            // Shell values should have been resolved before template rendering;
            // if one slips through, surface it clearly rather than silently
            // emitting an empty string.
            ResolvedValue::Shell(cmd)  => format!("shell({})", cmd),
        }
    }
}

/// Parse a value string and resolve it to a ResolvedValue.
/// shell() is NOT executed here — it is deferred to render time.
fn resolve_value(
    raw: &str,
    memory: &mut serde_json::Map<String, serde_json::Value>,
    memory_path: &Path,
) -> anyhow::Result<ResolvedValue> {
    let raw = raw.trim();

    // shell(...) — deferred to render time so the correct .envrc context
    // (KUBECONFIG etc.) is applied per platform file directory
    if let Some(cmd) = strip_wrapper(raw, "shell(") {
        return Ok(ResolvedValue::Shell(cmd.to_string()));
    }

    // onceshell(...) — run once eagerly, cache in values.memory.json.
    // These should not depend on per-directory cluster context; if they do,
    // use shell(...) instead.
    if let Some(cmd) = strip_wrapper(raw, "onceshell(") {
        let cache_key = format!("onceshell:{}", cmd);
        if let Some(cached) = memory.get(&cache_key).and_then(|v| v.as_str()) {
            println!("  ↩ onceshell cache hit: {}", &cmd[..cmd.len().min(40)]);
            return Ok(ResolvedValue::Concrete(cached.to_string()));
        }
        println!("  ⚙ onceshell executing: {}", &cmd[..cmd.len().min(40)]);
        let result = run_shell(cmd)?;
        memory.insert(cache_key, serde_json::Value::String(result.clone()));
        // persist memory immediately so partial runs are cached
        fs::write(
            memory_path,
            serde_json::to_string_pretty(&serde_json::Value::Object(memory.clone()))?,
        )?;
        return Ok(ResolvedValue::Concrete(result));
    }

    // file(...) — read file contents relative to CWD (paths start with ./)
    if let Some(path_str) = strip_wrapper(raw, "file(") {
        let path_str = path_str.trim();
        let content = fs::read_to_string(path_str)
            .map_err(|e| anyhow::anyhow!(
                "file({}) could not be read: {}",
                path_str, e
            ))?;
        // Don't trim_end() — SSH keys and certs require a trailing newline
        return Ok(ResolvedValue::Concrete(content));
    }

    // vault(...) — deferred, do not resolve now
    if let Some(key) = strip_wrapper(raw, "vault(") {
        return Ok(ResolvedValue::Vault(key.to_string()));
    }

    // env var interpolation — replace $VAR_NAME or ${VAR_NAME} with env value
    let env_re = Regex::new(r"\$\{?([A-Z_][A-Z0-9_]*)\}?").unwrap();
    if env_re.is_match(raw) {
        let mut errors: Vec<String> = Vec::new();
        let result = env_re.replace_all(raw, |caps: &regex::Captures| {
            let var_name = caps.get(1).map(|m| m.as_str()).unwrap_or("");
            match std::env::var(var_name) {
                Ok(val) => val,
                Err(_) => {
                    errors.push(var_name.to_string());
                    caps.get(0).map(|m| m.as_str()).unwrap_or("").to_string()
                }
            }
        }).to_string();
        if !errors.is_empty() {
            anyhow::bail!(
                "Undefined environment variable(s): {}",
                errors.join(", ")
            );
        }
        return Ok(ResolvedValue::Concrete(result));
    }

    // plain string
    Ok(ResolvedValue::Concrete(raw.to_string()))
}

fn strip_wrapper<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    if s.starts_with(prefix) && s.ends_with(')') {
        Some(&s[prefix.len()..s.len() - 1])
    } else {
        None
    }
}

/// Run a shell command with no extra env, used only by onceshell().
fn run_shell(cmd: &str) -> anyhow::Result<String> {
    let output = Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .output()
        .map_err(|e| anyhow::anyhow!("Failed to spawn shell: {}", e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("shell command failed:\n  cmd: {}\n  err: {}", cmd, stderr.trim());
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Run a shell command with a specific set of env vars injected.
/// Used at render time when we know the file's .envrc context.
fn run_shell_with_env(cmd: &str, env_vars: &HashMap<String, String>) -> anyhow::Result<String> {
    let kubeconfig = env_vars
        .get("KUBECONFIG")
        .cloned()
        .or_else(|| std::env::var("KUBECONFIG").ok())
        .unwrap_or_else(|| "(not set)".to_string());
    println!("    [shell] KUBECONFIG={}", kubeconfig);
    println!("    [shell] cmd: {}", &cmd[..cmd.len().min(60)]);

    let mut command = Command::new("sh");
    command.arg("-c").arg(cmd);
    for (k, v) in env_vars {
        command.env(k, v);
    }

    let output = command
        .output()
        .map_err(|e| anyhow::anyhow!("Failed to spawn shell: {}", e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("shell command failed:\n  cmd: {}\n  err: {}", cmd, stderr.trim());
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// For a given file's env context, resolve any Shell values into Concrete ones.
/// All other value types are passed through unchanged.
pub fn resolve_shell_values(
    resolved: &ResolvedValues,
    env_vars: &HashMap<String, String>,
) -> anyhow::Result<ResolvedValues> {
    let mut out = ResolvedValues::new();
    for (key, val) in resolved {
        let concrete = match val {
            ResolvedValue::Shell(cmd) => {
                print!("  resolving shell {}... ", key);
                let result = run_shell_with_env(cmd, env_vars)
                    .map_err(|e| anyhow::anyhow!("Failed to resolve shell({}) for key {}: {}", cmd, key, e))?;
                println!("✓");
                ResolvedValue::Concrete(result)
            }
            other => other.clone(),
        };
        out.insert(key.clone(), concrete);
    }
    Ok(out)
}

// ── Values loading and resolution ─────────────────────────────────────────────

/// Flat resolved map: "KEY" or "SECTION.KEY" or "A.B.C.D" → ResolvedValue
pub type ResolvedValues = HashMap<String, ResolvedValue>;

fn collect_keys(value: &serde_json::Value, prefix: &str, keys: &mut HashSet<String>) {
    match value {
        serde_json::Value::Object(map) => {
            for (k, v) in map {
                let full_key = if prefix.is_empty() { k.clone() } else { format!("{}.{}", prefix, k) };
                collect_keys(v, &full_key, keys);
            }
        }
        _ => { keys.insert(prefix.to_string()); }
    }
}

fn check_unfilled(value: &serde_json::Value, prefix: &str, unfilled: &mut Vec<String>) {
    match value {
        serde_json::Value::Object(map) => {
            for (k, v) in map {
                let full_key = if prefix.is_empty() { k.clone() } else { format!("{}.{}", prefix, k) };
                check_unfilled(v, &full_key, unfilled);
            }
        }
        serde_json::Value::Null => { unfilled.push(prefix.to_string()); }
        serde_json::Value::String(s) => {
            let t = s.trim();
            if (t.is_empty() || t == "CHANGE_ME")
                && !t.starts_with("shell(")
                && !t.starts_with("onceshell(")
                && !t.starts_with("file(")
                && !t.starts_with("vault(")
                && !t.contains('$')
            {
                unfilled.push(prefix.to_string());
            }
        }
        _ => {}
    }
}

/// Recursively walk the values JSON tree, resolving every leaf string.
fn resolve_recursive(
    value: &serde_json::Value,
    prefix: &str,
    memory: &mut serde_json::Map<String, serde_json::Value>,
    memory_path: &Path,
    resolved: &mut ResolvedValues,
) -> anyhow::Result<()> {
    match value {
        serde_json::Value::Object(map) => {
            for (k, v) in map {
                let key = if prefix.is_empty() {
                    k.clone()
                } else {
                    format!("{}.{}", prefix, k)
                };
                resolve_recursive(v, &key, memory, memory_path, resolved)?;
            }
        }
        serde_json::Value::String(s) => {
            print!("  resolving {}... ", prefix);
            let resolved_val = resolve_value(s, memory, memory_path)
                .map_err(|e| anyhow::anyhow!("Failed to resolve {}: {}", prefix, e))?;
            match &resolved_val {
                ResolvedValue::Concrete(_) => println!("✓"),
                ResolvedValue::Vault(k)    => println!("⏳ vault({}) — deferred to deploy", k),
                ResolvedValue::Shell(cmd)  => println!("⏳ shell({}) — deferred to render", &cmd[..cmd.len().min(40)]),
            }
            resolved.insert(prefix.to_string(), resolved_val);
        }
        other => {
            // numbers, bools — convert to string as-is
            print!("  resolving {}... ", prefix);
            println!("✓");
            resolved.insert(prefix.to_string(), ResolvedValue::Concrete(other.to_string()));
        }
    }
    Ok(())
}

pub fn load_and_resolve_values(
    values_path: &Path,
    backup_path: &Path,
    memory_path: &Path,
) -> anyhow::Result<ResolvedValues> {
    if values_path == backup_path {
        anyhow::bail!("values.json and values.json.backup cannot be the same file.");
    }

    if !backup_path.exists() {
        anyhow::bail!(
            "values.json.backup not found — must be committed to git.\n\
             It defines the expected shape of values.json."
        );
    }
    let backup: serde_json::Value = serde_json::from_str(&fs::read_to_string(backup_path)?)
        .map_err(|e| anyhow::anyhow!("Failed to parse values.json.backup: {e}"))?;

    if !values_path.exists() {
        anyhow::bail!(
            "values.json not found.\n\
             Copy values.json.backup → values.json and fill in real values.\n\
             values.json is gitignored — never commit it."
        );
    }
    let values: serde_json::Value = serde_json::from_str(&fs::read_to_string(values_path)?)
        .map_err(|e| anyhow::anyhow!("Failed to parse values.json: {e}"))?;

    // key sync check
    let mut backup_keys: HashSet<String> = HashSet::new();
    let mut values_keys: HashSet<String> = HashSet::new();
    collect_keys(&backup, "", &mut backup_keys);
    collect_keys(&values, "", &mut values_keys);

    let mut missing: Vec<_> = backup_keys.difference(&values_keys).cloned().collect();
    let mut extra: Vec<_>   = values_keys.difference(&backup_keys).cloned().collect();
    missing.sort(); extra.sort();

    let mut has_errors = false;
    if !missing.is_empty() {
        has_errors = true;
        eprintln!("\n✗ Keys in values.json.backup MISSING from values.json:");
        for k in &missing { eprintln!("    - {}", k); }
        eprintln!("\n  Add them to values.json before running plan.");
    }
    if !extra.is_empty() {
        has_errors = true;
        eprintln!("\n✗ Keys in values.json NOT in values.json.backup:");
        for k in &extra { eprintln!("    - {}", k); }
        eprintln!("\n  Remove or add to values.json.backup and commit.");
    }
    if has_errors {
        anyhow::bail!("values.json and values.json.backup are out of sync.");
    }

    // unfilled check
    let mut unfilled: Vec<String> = Vec::new();
    check_unfilled(&values, "", &mut unfilled);
    unfilled.sort();
    if !unfilled.is_empty() {
        eprintln!("\n✗ Unfilled values in values.json:");
        for k in &unfilled { eprintln!("    - {}", k); }
        anyhow::bail!("Fill in all placeholder values before running plan.");
    }

    println!("✓ values.json validated ({} keys)", backup_keys.len());

    // load memory cache
    let mut memory: serde_json::Map<String, serde_json::Value> = if memory_path.exists() {
        let raw = fs::read_to_string(memory_path)?;
        serde_json::from_str::<serde_json::Value>(&raw)
            .ok()
            .and_then(|v| if let serde_json::Value::Object(m) = v { Some(m) } else { None })
            .unwrap_or_default()
    } else {
        serde_json::Map::new()
    };

    // resolve all values recursively — shell() stays deferred
    let mut resolved: ResolvedValues = ResolvedValues::new();
    resolve_recursive(&values, "", &mut memory, memory_path, &mut resolved)?;

    // persist final memory
    fs::write(
        memory_path,
        serde_json::to_string_pretty(&serde_json::Value::Object(memory))?,
    )?;

    Ok(resolved)
}

// ── Template rendering ────────────────────────────────────────────────────────

/// Substitute {{KEY}} or {{SECTION.KEY}} or {{A.B.C}} in template content.
/// By the time this is called, all Shell values must already be resolved to
/// Concrete via resolve_shell_values() — if any Shell slips through,
/// as_template_str() will emit shell(...) visibly rather than silently empty.
pub fn render_template(
    content: &str,
    template_name: &str,
    resolved: &ResolvedValues,
    snapshot: &Snapshot,
) -> anyhow::Result<String> {
    use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};

    let ref_re = Regex::new(
        r"\{\{\s*([A-Za-z0-9_]+(?:\.[A-Za-z0-9_]+)*)\s*(?:\|\s*(b64encode))?\s*\}\}"
    ).unwrap();
    let svc_re = Regex::new(r#"\{\{\s*services\["([^"]+)"\]\s*\}\}"#).unwrap();
    let pkg_re = Regex::new(r#"\{\{\s*packages\["([^"]+)"\]\s*\}\}"#).unwrap();
    let db_re  = Regex::new(r#"\{\{\s*databases\["([^"]+)"\]\s*\}\}"#).unwrap();

    // ── validate all refs first ───────────────────────────────────────────────
    let mut errors: Vec<String> = Vec::new();

    for cap in ref_re.captures_iter(content) {
        if let Some(key) = cap.get(1).map(|m| m.as_str()) {
            if !resolved.contains_key(key) {
                errors.push(format!("  {{{{{}}}}} — not found in values.json", key));
            }
        }
    }
    let svc_ids: HashSet<_> = snapshot.services.iter().map(|s| s.identifier.as_str()).collect();
    for cap in svc_re.captures_iter(content) {
        if let Some(id) = cap.get(1).map(|m| m.as_str()) {
            if !svc_ids.contains(id) {
                errors.push(format!("  services[\"{}\"] — not in snapshot.json", id));
            }
        }
    }
    let pkg_ids: HashSet<_> = snapshot.packages.iter().map(|p| p.identifier.as_str()).collect();
    for cap in pkg_re.captures_iter(content) {
        if let Some(id) = cap.get(1).map(|m| m.as_str()) {
            if !pkg_ids.contains(id) {
                errors.push(format!("  packages[\"{}\"] — not in snapshot.json", id));
            }
        }
    }
    let db_names: HashSet<_> = snapshot.databases.iter().map(|d| d.name.as_str()).collect();
    for cap in db_re.captures_iter(content) {
        if let Some(name) = cap.get(1).map(|m| m.as_str()) {
            if !db_names.contains(name) {
                errors.push(format!("  databases[\"{}\"] — not in snapshot.json", name));
            }
        }
    }

    if !errors.is_empty() {
        anyhow::bail!(
            "Template '{}' has undefined references:\n{}",
            template_name, errors.join("\n")
        );
    }

    // ── substitute value refs ─────────────────────────────────────────────────
    let mut result = ref_re.replace_all(content, |caps: &regex::Captures| {
        let key    = caps.get(1).map(|m| m.as_str()).unwrap_or("");
        let filter = caps.get(2).map(|m| m.as_str()).unwrap_or("");

        let value_str = match resolved.get(key) {
            Some(v) => v.as_template_str(),
            None    => return format!("{{{{{}}}}}", key),
        };

        match filter {
            "b64encode" => BASE64.encode(value_str.as_bytes()),
            _           => value_str,
        }
    }).to_string();

    // ── substitute snapshot refs ──────────────────────────────────────────────
    result = svc_re.replace_all(&result, |caps: &regex::Captures| {
        let id = caps.get(1).map(|m| m.as_str()).unwrap_or("");
        snapshot.services.iter().find(|s| s.identifier == id)
            .map(|s| s.version.clone()).unwrap_or_default()
    }).to_string();

    result = pkg_re.replace_all(&result, |caps: &regex::Captures| {
        let id = caps.get(1).map(|m| m.as_str()).unwrap_or("");
        snapshot.packages.iter().find(|p| p.identifier == id)
            .map(|p| p.version.clone()).unwrap_or_default()
    }).to_string();

    result = db_re.replace_all(&result, |caps: &regex::Captures| {
        let name = caps.get(1).map(|m| m.as_str()).unwrap_or("");
        snapshot.databases.iter().find(|d| d.name == name)
            .map(|d| d.version.clone()).unwrap_or_default()
    }).to_string();

    Ok(result)
}

// ── File discovery ────────────────────────────────────────────────────────────

#[derive(Debug, PartialEq)]
pub enum PlatformFileType {
    Yaml,
    Secrets,
    Envrc,
}

#[derive(Debug)]
pub struct PlatformFile {
    pub absolute_path: PathBuf,
    pub relative_path: PathBuf,
    pub file_type: PlatformFileType,
}

fn discover_platform_files(platform_dir: &Path) -> Vec<PlatformFile> {
    let mut files = Vec::new();

    for entry in WalkDir::new(platform_dir)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
    {
        let path = entry.path().to_path_buf();
        let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");

        let file_type = if file_name == "secrets.yaml" {
            PlatformFileType::Secrets
        } else if file_name == ".envrc" {
            PlatformFileType::Envrc
        } else if file_name.ends_with(".yaml") || file_name.ends_with(".yml") {
            PlatformFileType::Yaml
        } else {
            continue;
        };

        let relative_path = path.strip_prefix(platform_dir).unwrap_or(&path).to_path_buf();

        files.push(PlatformFile { absolute_path: path, relative_path, file_type });
    }

    files.sort_by(|a, b| a.relative_path.cmp(&b.relative_path));
    files
}

// ── Build output ──────────────────────────────────────────────────────────────

fn render_to_build(
    files: &[PlatformFile],
    platform_dir: &Path,
    build_dir: &Path,
    resolved: &ResolvedValues,
    snapshot: &Snapshot,
) -> anyhow::Result<()> {
    use crate::run_dry_run::{find_envrc_bounded, parse_envrc};

    if build_dir.exists() {
        fs::remove_dir_all(build_dir)?;
    }

    // We walk up from each file's directory bounded by platform/ to find the
    // nearest .envrc. This means platform/mcs/secrets.yaml picks up
    // platform/mcs/.envrc (with its KUBECONFIG), not the root one.
    let platform_dir_canonical = platform_dir.canonicalize()?;

    for file in files {
        let dest = build_dir.join(&file.relative_path);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }

        let raw = fs::read_to_string(&file.absolute_path)
            .map_err(|e| anyhow::anyhow!("Cannot read '{}': {e}", file.absolute_path.display()))?;

        let template_name = file.relative_path.to_string_lossy().to_string();

        let rendered = if file.file_type == PlatformFileType::Envrc {
            // .envrc is never templated — copy verbatim
            raw
        } else if raw.contains("{{") {
            // Find the nearest .envrc walking up from this file's directory,
            // bounded by platform/. This gives us the correct KUBECONFIG for
            // any shell() commands that reference cluster secrets.
            let file_dir = file.absolute_path
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| platform_dir.to_path_buf());

            let env_vars = match find_envrc_bounded(&file_dir, &platform_dir_canonical) {
                Some(envrc_path) => {
                    println!(
                        "  .envrc context for {}: {}",
                        file.relative_path.display(),
                        envrc_path.display()
                    );
                    let content = fs::read_to_string(&envrc_path).map_err(|e| {
                        anyhow::anyhow!("Cannot read '{}': {e}", envrc_path.display())
                    })?;
                    parse_envrc(&content)
                }
                None => {
                    // No .envrc found in platform/ — fall back to process env.
                    // shell() commands will inherit whatever KUBECONFIG is set
                    // in the environment that launched ginger-infra.
                    HashMap::new()
                }
            };

            // Resolve shell() values now that we have the correct env context
            let contextual_resolved = resolve_shell_values(resolved, &env_vars)?;

            render_template(&raw, &template_name, &contextual_resolved, snapshot)?
        } else {
            raw
        };

        fs::write(&dest, rendered)
            .map_err(|e| anyhow::anyhow!("Cannot write '{}': {e}", dest.display()))?;

        println!("  → build/{}", file.relative_path.display());
    }

    Ok(())
}

// ── Public entry point ────────────────────────────────────────────────────────

pub fn run_plan() -> anyhow::Result<()> {
    // 1. gitignore check — always first
    check_gitignore()?;

    // 2. load + validate snapshot
    let snapshot_path = Path::new("snapshot.json");
    if !snapshot_path.exists() {
        anyhow::bail!("snapshot.json not found in current directory");
    }
    let snapshot: Snapshot = serde_json::from_str(&fs::read_to_string(snapshot_path)?)
        .map_err(|e| anyhow::anyhow!("Failed to parse snapshot.json: {e}"))?;

    println!("✓ Loaded snapshot.json");
    println!(
        "  services: {}  packages: {}  databases: {}",
        snapshot.services.len(), snapshot.packages.len(), snapshot.databases.len()
    );
    validate_snapshot(&snapshot)?;

    // 3. load + resolve values
    // Note: shell() values are NOT executed here — they are deferred to
    // render time (step 5) where we know which .envrc context applies.
    println!("\n── Resolving values ─────────────────────────────────");
    let resolved = load_and_resolve_values(
        Path::new("values.json"),
        Path::new("values.json.backup"),
        Path::new("values.memory.json"),
    )?;

    let concrete_count = resolved.values().filter(|v| matches!(v, ResolvedValue::Concrete(_))).count();
    let vault_count    = resolved.values().filter(|v| matches!(v, ResolvedValue::Vault(_))).count();
    let shell_count    = resolved.values().filter(|v| matches!(v, ResolvedValue::Shell(_))).count();
    println!(
        "\n✓ Values resolved: {} concrete, {} vault (deferred to deploy), {} shell (deferred to render)",
        concrete_count, vault_count, shell_count
    );

    // 4. discover platform files
    let platform_dir = Path::new("platform");
    if !platform_dir.exists() {
        anyhow::bail!("platform/ directory not found");
    }
    let files = discover_platform_files(platform_dir);
    if files.is_empty() {
        anyhow::bail!("platform/ is empty — no .yaml/.yml/secrets.yaml files found");
    }

    let yaml_count    = files.iter().filter(|f| f.file_type == PlatformFileType::Yaml).count();
    let secrets_count = files.iter().filter(|f| f.file_type == PlatformFileType::Secrets).count();
    let envrc_count   = files.iter().filter(|f| f.file_type == PlatformFileType::Envrc).count();

    println!("\n── Platform files ───────────────────────────────────");
    println!("  {} yaml/yml  |  {} secrets.yaml  |  {} .envrc", yaml_count, secrets_count, envrc_count);

    // 5. render to build/
    // shell() values are resolved here, scoped to each file's nearest .envrc
    // within platform/ — so platform/mcs/**  picks up platform/mcs/.envrc
    // and gets the correct KUBECONFIG for that cluster.
    println!("\n── Rendering to build/ ──────────────────────────────");
    render_to_build(&files, platform_dir, Path::new("build"), &resolved, &snapshot)?;

    println!("\n✓ Rendered {} files → build/", files.len());
    if vault_count > 0 {
        println!(
            "  ⚠ {} vault values left as placeholders — will be resolved at deploy time",
            vault_count
        );
    }
    println!("\nNext: review build/ then run `ginger-infra deploy`");

    Ok(())
}