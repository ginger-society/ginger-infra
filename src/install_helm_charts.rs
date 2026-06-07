use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Deserialize;

use crate::plan::{
    load_and_resolve_values, resolve_shell_values, ResolvedValue, ResolvedValues, Snapshot,
};
use crate::run_dry_run::{find_envrc_bounded, parse_envrc};

// ── Resource types ────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Resource {
    LocalChart(LocalChart),
    RemoteChart(RemoteChart),
}

#[derive(Debug, Deserialize)]
struct LocalChart {
    release: String,
    /// Path to the chart directory, relative to the resources.toml file
    chart: String,
    #[serde(default)]
    set: HashMap<String, String>,
    #[serde(default = "default_timeout")]
    timeout: String,
    #[serde(default)]
    namespace: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RemoteChart {
    release: String,
    repo_name: String,
    repo_url: String,
    /// "<repo_name>/<chart_name>" e.g. "bitnami/postgresql"
    chart: String,
    /// Optional values file, relative to the resources.toml file
    values_file: Option<String>,
    #[serde(default)]
    set: HashMap<String, String>,
    #[serde(default = "default_timeout")]
    timeout: String,
    #[serde(default)]
    namespace: Option<String>,
}

fn default_timeout() -> String {
    "5m".to_string()
}

#[derive(Debug, Deserialize)]
struct ResourcesFile {
    resource: Vec<Resource>,
}

// ── Discovery ─────────────────────────────────────────────────────────────────

/// Find all resources.toml files directly under platform/<cluster>/ directories.
fn discover_resources_files(platform_dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();

    let entries = match fs::read_dir(platform_dir) {
        Ok(e) => e,
        Err(_) => return found,
    };

    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        if path.is_dir() {
            let candidate = path.join("resources.toml");
            if candidate.is_file() {
                found.push(candidate);
            }
        }
    }

    found.sort();
    found
}

// ── Template substitution for --set values ────────────────────────────────────

/// Substitute {{KEY}} placeholders in a single string using resolved values.
/// This mirrors render_template() but for a single value string rather than
/// a full file.
///
/// - Concrete  → substituted immediately
/// - Vault     → emitted as `vault(KEY)` literal, substituted at install time
/// - Shell     → should already be Concrete by this point (resolve_shell_values
///               runs before this); error if one slips through
/// - Missing   → error
fn substitute_set_value(raw: &str, resolved: &ResolvedValues, key_hint: &str) -> anyhow::Result<String> {
    use regex::Regex;
    let re = Regex::new(r"\{\{\s*([A-Za-z0-9_]+(?:\.[A-Za-z0-9_]+)*)\s*\}\}").unwrap();

    let mut errors: Vec<String> = Vec::new();

    let result = re.replace_all(raw, |caps: &regex::Captures| {
        let k = caps.get(1).map(|m| m.as_str()).unwrap_or("");
        match resolved.get(k) {
            Some(ResolvedValue::Concrete(v)) => v.clone(),
            Some(ResolvedValue::Vault(vault_key)) => {
                // pass through as vault(KEY) — substituted in-memory at install time
                format!("vault({})", vault_key)
            }
            Some(ResolvedValue::Shell(_)) => {
                // should have been resolved by resolve_shell_values() before this call
                errors.push(format!(
                    "'{{{{{}}}}}' still has an unresolved shell() value in set.{} — this is a bug",
                    k, key_hint
                ));
                String::new()
            }
            None => {
                errors.push(format!("'{{{{{}}}}}' not found in values.json (used in set.{})", k, key_hint));
                String::new()
            }
        }
    }).to_string();

    if !errors.is_empty() {
        anyhow::bail!("{}", errors.join("\n"));
    }

    Ok(result)
}

// ── Vault substitution (mirrors rollout.rs) ───────────────────────────────────

fn substitute_vault_str(raw: &str, vault: &HashMap<String, String>, hint: &str) -> anyhow::Result<String> {
    use regex::Regex;
    let re = Regex::new(r"vault\(([^)]+)\)").unwrap();
    let mut errors: Vec<String> = Vec::new();

    let result = re.replace_all(raw, |caps: &regex::Captures| {
        let key = caps.get(1).map(|m| m.as_str().trim()).unwrap_or("");
        match vault.get(key) {
            Some(v) => v.clone(),
            None => {
                errors.push(format!("vault key '{}' not found in vault.json (in {})", key, hint));
                format!("vault({})", key)
            }
        }
    }).to_string();

    if !errors.is_empty() {
        anyhow::bail!("{}", errors.join("\n"));
    }

    Ok(result)
}

// ── helm repo add (idempotent, warnings suppressed) ───────────────────────────

fn helm_repo_add(repo_name: &str, repo_url: &str, env_vars: &HashMap<String, String>) -> anyhow::Result<()> {
    let output = Command::new("helm")
        .args(["repo", "add", repo_name, repo_url])
        .envs(env_vars)
        .output()
        .map_err(|e| anyhow::anyhow!("Failed to run helm repo add: {}", e))?;

    // exit 1 with "already exists" is fine — idempotent
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("already exists") {
            // silently ignore
            return Ok(());
        }
        anyhow::bail!(
            "helm repo add '{}' failed (exit {:?}): {}",
            repo_name,
            output.status.code(),
            stderr.trim()
        );
    }

    println!("  ✓ helm repo '{}' added", repo_name);
    Ok(())
}

fn helm_repo_update(env_vars: &HashMap<String, String>) -> anyhow::Result<()> {
    let output = Command::new("helm")
        .args(["repo", "update"])
        .envs(env_vars)
        .output()
        .map_err(|e| anyhow::anyhow!("Failed to run helm repo update: {}", e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("helm repo update failed: {}", stderr.trim());
    }

    println!("  ✓ helm repo update complete");
    Ok(())
}

// ── helm upgrade --install ────────────────────────────────────────────────────

fn helm_upgrade_install(
    release: &str,
    chart: &str,
    set_values: &HashMap<String, String>,    // already fully resolved
    values_file: Option<&str>,
    namespace: Option<&str>,
    timeout: &str,
    env_vars: &HashMap<String, String>,
    label: &str,
) -> anyhow::Result<()> {
    let mut cmd = Command::new("helm");
    cmd.args(["upgrade", "--install", release, chart]);
    cmd.args(["--wait", "--timeout", timeout]);

    if let Some(ns) = namespace {
        cmd.args(["--namespace", ns, "--create-namespace"]);
    }

    if let Some(vf) = values_file {
        cmd.args(["-f", vf]);
    }

    // pass --set values in sorted order for determinism
    let mut set_pairs: Vec<_> = set_values.iter().collect();
    set_pairs.sort_by_key(|(k, _)| k.as_str());
    for (k, v) in &set_pairs {
        cmd.args(["--set", &format!("{}={}", k, v)]);
    }

    cmd.envs(env_vars);

    println!("  ⚙ helm upgrade --install {} {} (--wait --timeout {})", release, chart, timeout);
    for (k, v) in &set_pairs {
        // mask anything that looks like a password
        let display = if k.to_lowercase().contains("password") || k.to_lowercase().contains("secret") {
            "***".to_string()
        } else {
            v.to_string()
        };
        println!("      --set {}={}", k, display);
    }

    let output = cmd
        .output()
        .map_err(|e| anyhow::anyhow!("Failed to spawn helm: {}", e))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    if output.status.success() {
        for line in stdout.lines() {
            println!("    {}", line);
        }
        println!("  ✓ {} installed/upgraded successfully", label);
        Ok(())
    } else {
        eprintln!("  ✗ {} — helm failed (exit {:?})", label, output.status.code());
        for line in stderr.lines() {
            eprintln!("    {}", line);
        }
        anyhow::bail!("helm upgrade --install failed for '{}'", release);
    }
}

// ── Plan step: render resources.toml → helm-build/ ───────────────────────────

/// Render all resources.toml files found under platform/ into helm-build/,
/// resolving shell() values with the correct .envrc context.
/// vault() placeholders remain as-is for the install step.
pub fn plan_helm_charts() -> anyhow::Result<()> {
    let platform_dir = Path::new("platform");
    if !platform_dir.exists() {
        anyhow::bail!("platform/ directory not found");
    }

    let resources_files = discover_resources_files(platform_dir);

    if resources_files.is_empty() {
        println!("No resources.toml files found under platform/");
        return Ok(());
    }

    // load + resolve values (shell() deferred, vault() deferred)
    println!("── Resolving values ─────────────────────────────────");
    let resolved = load_and_resolve_values(
        Path::new("values.json"),
        Path::new("values.json.backup"),
        Path::new("values.memory.json"),
    )?;

    // load snapshot (needed by load_and_resolve_values' render path)
    let snapshot_path = Path::new("snapshot.json");
    if !snapshot_path.exists() {
        anyhow::bail!("snapshot.json not found");
    }
    let snapshot: Snapshot = serde_json::from_str(&fs::read_to_string(snapshot_path)?)
        .map_err(|e| anyhow::anyhow!("Failed to parse snapshot.json: {e}"))?;

    let helm_build_dir = Path::new("helm-build");
    if helm_build_dir.exists() {
        fs::remove_dir_all(helm_build_dir)?;
    }
    fs::create_dir_all(helm_build_dir)?;

    let platform_canonical = platform_dir.canonicalize()?;

    println!("\n── Rendering helm-build/ ────────────────────────────");

    for resources_path in &resources_files {
        let cluster_dir = resources_path.parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| platform_dir.to_path_buf());

        let cluster_label = cluster_dir
            .strip_prefix(platform_dir)
            .unwrap_or(&cluster_dir)
            .display()
            .to_string();

        // resolve .envrc for shell() context
        let env_vars = match find_envrc_bounded(&cluster_dir, &platform_canonical) {
            Some(envrc_path) => {
                let content = fs::read_to_string(&envrc_path)
                    .map_err(|e| anyhow::anyhow!("Cannot read .envrc: {}", e))?;
                parse_envrc(&content)
            }
            None => HashMap::new(),
        };

        // resolve shell() values with this cluster's env context
        let contextual_resolved = resolve_shell_values(&resolved, &env_vars)?;

        // parse and validate the resources.toml
        let toml_content = fs::read_to_string(resources_path)
            .map_err(|e| anyhow::anyhow!("Cannot read {}: {}", resources_path.display(), e))?;

        let resources_file: ResourcesFile = toml::from_str(&toml_content)
            .map_err(|e| anyhow::anyhow!("Failed to parse {}: {}", resources_path.display(), e))?;

        // render each resource's set values — vault() stays as-is, shell()/{{}} resolved
        let rendered = render_resources_toml(&resources_file, &contextual_resolved, &cluster_label)?;

        // write to helm-build/<cluster>/resources.toml
        let dest_dir = helm_build_dir.join(&cluster_label);
        fs::create_dir_all(&dest_dir)?;
        let dest = dest_dir.join("resources.toml");
        fs::write(&dest, rendered)
            .map_err(|e| anyhow::anyhow!("Cannot write {}: {}", dest.display(), e))?;

        println!("  → helm-build/{}/resources.toml", cluster_label);

        // copy .envrc verbatim so find_envrc_bounded works in helm-build/ at install time,
        // exactly as plan.rs does for build/ — without this the install step falls back to
        // inherited environment and loses KUBECONFIG
        match find_envrc_bounded(&cluster_dir, &platform_canonical) {
            Some(envrc_path) => {
                let envrc_dest = dest_dir.join(".envrc");
                fs::copy(&envrc_path, &envrc_dest).map_err(|e| anyhow::anyhow!(
                    "Cannot copy .envrc '{}' → '{}': {}",
                    envrc_path.display(), envrc_dest.display(), e
                ))?;
                println!("  → helm-build/{}/.envrc", cluster_label);
            }
            None => {
                println!("  ⚠ No .envrc found for {} — KUBECONFIG must be in environment at install time", cluster_label);
            }
        }

        // also copy any values files referenced by remote charts, resolving their paths
        copy_referenced_values_files(&resources_file, &cluster_dir, &dest_dir)?;
    }

    println!("\n✓ helm-build/ ready");
    Ok(())
}

/// Render a ResourcesFile back to TOML string with {{KEY}} substituted in set values.
/// vault() placeholders in set values are left as-is for the install step.
fn render_resources_toml(
    file: &ResourcesFile,
    resolved: &ResolvedValues,
    cluster_label: &str,
) -> anyhow::Result<String> {
    // We re-serialize to TOML manually rather than depending on serde round-trip,
    // so we can apply substitution only to set values while leaving everything else
    // exactly as written.
    let mut out = String::new();

    for resource in &file.resource {
        match resource {
            Resource::LocalChart(r) => {
                out.push_str("[[resource]]\n");
                out.push_str("type = \"local_chart\"\n");
                out.push_str(&format!("release = {:?}\n", r.release));
                out.push_str(&format!("chart = {:?}\n", r.chart));
                out.push_str(&format!("timeout = {:?}\n", r.timeout));
                if let Some(ns) = &r.namespace {
                    out.push_str(&format!("namespace = {:?}\n", ns));
                }
                for (k, v) in &r.set {
                    let rendered = substitute_set_value(v, resolved, k)
                        .map_err(|e| anyhow::anyhow!("[{}] local_chart '{}': {}", cluster_label, r.release, e))?;
                    out.push_str(&format!("set.{} = {:?}\n", quote_key(k), rendered));
                }
                out.push('\n');
            }
            Resource::RemoteChart(r) => {
                out.push_str("[[resource]]\n");
                out.push_str("type = \"remote_chart\"\n");
                out.push_str(&format!("release = {:?}\n", r.release));
                out.push_str(&format!("repo_name = {:?}\n", r.repo_name));
                out.push_str(&format!("repo_url = {:?}\n", r.repo_url));
                out.push_str(&format!("chart = {:?}\n", r.chart));
                out.push_str(&format!("timeout = {:?}\n", r.timeout));
                if let Some(ns) = &r.namespace {
                    out.push_str(&format!("namespace = {:?}\n", ns));
                }
                if let Some(vf) = &r.values_file {
                    // path will be rewritten to be relative to helm-build/<cluster>/
                    out.push_str(&format!("values_file = {:?}\n", Path::new(vf).file_name().unwrap_or_default().to_string_lossy().as_ref()));
                }
                for (k, v) in &r.set {
                    let rendered = substitute_set_value(v, resolved, k)
                        .map_err(|e| anyhow::anyhow!("[{}] remote_chart '{}': {}", cluster_label, r.release, e))?;
                    out.push_str(&format!("set.{} = {:?}\n", quote_key(k), rendered));
                }
                out.push('\n');
            }
        }
    }

    Ok(out)
}

/// Keys like "rabbitmq.auth.username" need quoting in TOML inline table keys.
fn quote_key(k: &str) -> String {
    if k.contains('.') || k.contains('-') {
        format!("\"{}\"", k)
    } else {
        k.to_string()
    }
}

/// Copy values files referenced by remote charts into the helm-build cluster dir.
fn copy_referenced_values_files(
    file: &ResourcesFile,
    source_dir: &Path,
    dest_dir: &Path,
) -> anyhow::Result<()> {
    for resource in &file.resource {
        if let Resource::RemoteChart(r) = resource {
            if let Some(vf) = &r.values_file {
                let src = source_dir.join(vf);
                let file_name = Path::new(vf).file_name()
                    .ok_or_else(|| anyhow::anyhow!("Invalid values_file path: {}", vf))?;
                let dst = dest_dir.join(file_name);

                fs::copy(&src, &dst).map_err(|e| anyhow::anyhow!(
                    "Cannot copy values file '{}' → '{}': {}",
                    src.display(), dst.display(), e
                ))?;
                println!("  → helm-build/{}", dst.display());
            }
        }
    }
    Ok(())
}

// ── Install step: read helm-build/, vault-substitute, run helm ────────────────

pub fn run_install_helm_charts() -> anyhow::Result<()> {
    // ── 1. plan step ─────────────────────────────────────────────────────────
    println!("── Planning helm charts ─────────────────────────────");
    plan_helm_charts()?;

    // ── 2. load vault.json ────────────────────────────────────────────────────
    let vault_path = Path::new("vault.json");
    let vault: HashMap<String, String> = if vault_path.exists() {
        let raw = fs::read_to_string(vault_path)?;
        serde_json::from_str(&raw)
            .map_err(|e| anyhow::anyhow!("Failed to parse vault.json: {e}"))?
    } else {
        println!("  ⚠ vault.json not found — vault() values will fail if referenced");
        HashMap::new()
    };

    // ── 3. collect rendered resources files ───────────────────────────────────
    let helm_build_dir = Path::new("helm-build");
    if !helm_build_dir.exists() {
        anyhow::bail!("helm-build/ not found — plan step must have failed");
    }

    let mut resources_files: Vec<PathBuf> = Vec::new();
    if let Ok(entries) = fs::read_dir(helm_build_dir) {
        for entry in entries.filter_map(|e| e.ok()) {
            let p = entry.path().join("resources.toml");
            if p.is_file() {
                resources_files.push(p);
            }
        }
    }
    resources_files.sort();

    if resources_files.is_empty() {
        println!("No resources.toml files in helm-build/ — nothing to install");
        return Ok(());
    }

    println!("\n── Installing helm charts ───────────────────────────");

    let helm_build_canonical = helm_build_dir.canonicalize()?;
    let mut installed: Vec<String> = Vec::new();
    let mut failed: Vec<String> = Vec::new();

    // ── 4. process each cluster ───────────────────────────────────────────────
    for resources_path in &resources_files {
        let cluster_dir = resources_path.parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| helm_build_dir.to_path_buf());

        let cluster_label = cluster_dir
            .strip_prefix(helm_build_dir)
            .unwrap_or(&cluster_dir)
            .display()
            .to_string();

        println!("\n── {} ──────────────────────────────────────────", cluster_label);

        // resolve .envrc for KUBECONFIG (walk up from helm-build/<cluster>/ bounded by helm-build/)
        let env_vars = match find_envrc_bounded(&cluster_dir, &helm_build_canonical) {
            Some(envrc_path) => {
                let content = fs::read_to_string(&envrc_path)
                    .map_err(|e| anyhow::anyhow!("Cannot read .envrc: {}", e))?;
                let vars = parse_envrc(&content);
                println!("  ✓ .envrc loaded from {}", envrc_path.display());
                vars
            }
            None => {
                println!("  ⚠ No .envrc found — using inherited environment");
                HashMap::new()
            }
        };

        // parse rendered resources.toml
        let toml_content = fs::read_to_string(resources_path)
            .map_err(|e| anyhow::anyhow!("Cannot read {}: {}", resources_path.display(), e))?;

        let resources_file: ResourcesFile = toml::from_str(&toml_content)
            .map_err(|e| anyhow::anyhow!("Failed to parse {}: {}", resources_path.display(), e))?;

        // collect remote repos to add
        let mut repos_to_add: Vec<(&str, &str)> = Vec::new();
        let mut seen_repos: HashSet<&str> = HashSet::new();
        for resource in &resources_file.resource {
            if let Resource::RemoteChart(r) = resource {
                if seen_repos.insert(r.repo_name.as_str()) {
                    repos_to_add.push((&r.repo_name, &r.repo_url));
                }
            }
        }

        // add repos + update once
        if !repos_to_add.is_empty() {
            for (name, url) in &repos_to_add {
                if let Err(e) = helm_repo_add(name, url, &env_vars) {
                    // fatal — bail immediately
                    anyhow::bail!("[{}] helm repo add failed: {}", cluster_label, e);
                }
            }
            helm_repo_update(&env_vars)
                .map_err(|e| anyhow::anyhow!("[{}] {}", cluster_label, e))?;
        }

        // ── 5. install each resource ──────────────────────────────────────────
        for resource in &resources_file.resource {
            match resource {
                Resource::LocalChart(r) => {
                    let label = format!("{}/{}", cluster_label, r.release);

                    // resolve vault() in set values
                    let mut resolved_set: HashMap<String, String> = HashMap::new();
                    let mut vault_err = false;
                    for (k, v) in &r.set {
                        match substitute_vault_str(v, &vault, &label) {
                            Ok(s) => { resolved_set.insert(k.clone(), s); }
                            Err(e) => {
                                eprintln!("  ✗ {}: {}", label, e);
                                vault_err = true;
                            }
                        }
                    }
                    if vault_err {
                        anyhow::bail!("vault substitution failed for '{}'", label);
                    }

                    // chart path is relative to resources.toml in helm-build/<cluster>/
                    // but the original path was relative to platform/<cluster>/
                    // We stored it as-is and resolve relative to platform/ at install time
                    let chart_path = Path::new("platform")
                        .join(&cluster_label)
                        .join(&r.chart);
                    let chart_str = chart_path.to_string_lossy().to_string();

                    match helm_upgrade_install(
                        &r.release,
                        &chart_str,
                        &resolved_set,
                        None,
                        r.namespace.as_deref(),
                        &r.timeout,
                        &env_vars,
                        &label,
                    ) {
                        Ok(()) => installed.push(label),
                        Err(e) => anyhow::bail!("{}", e),   // fatal
                    }
                }

                Resource::RemoteChart(r) => {
                    let label = format!("{}/{}", cluster_label, r.release);

                    // resolve vault() in set values
                    let mut resolved_set: HashMap<String, String> = HashMap::new();
                    let mut vault_err = false;
                    for (k, v) in &r.set {
                        match substitute_vault_str(v, &vault, &label) {
                            Ok(s) => { resolved_set.insert(k.clone(), s); }
                            Err(e) => {
                                eprintln!("  ✗ {}: {}", label, e);
                                vault_err = true;
                            }
                        }
                    }
                    if vault_err {
                        anyhow::bail!("vault substitution failed for '{}'", label);
                    }

                    // values_file path is relative to helm-build/<cluster>/
                    let values_file = r.values_file.as_ref().map(|vf| {
                        cluster_dir.join(Path::new(vf).file_name().unwrap_or_default())
                            .to_string_lossy()
                            .to_string()
                    });

                    match helm_upgrade_install(
                        &r.release,
                        &r.chart,
                        &resolved_set,
                        values_file.as_deref(),
                        r.namespace.as_deref(),
                        &r.timeout,
                        &env_vars,
                        &label,
                    ) {
                        Ok(()) => installed.push(label),
                        Err(e) => anyhow::bail!("{}", e),   // fatal
                    }
                }
            }
        }
    }

    // ── 6. summary ────────────────────────────────────────────────────────────
    println!("\n── Summary ──────────────────────────────────────────");

    if !installed.is_empty() {
        println!("  ✓ Installed ({}):", installed.len());
        for r in &installed { println!("      {}", r); }
    }
    if failed.is_empty() {
        println!("\n  ✓ All helm charts installed and ready.");
    }

    println!();
    Ok(())
}