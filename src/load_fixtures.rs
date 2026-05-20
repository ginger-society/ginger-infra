use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use serde::Deserialize;
use walkdir::WalkDir;

use crate::run_dry_run::{find_envrc_bounded, parse_envrc};

// ── Fixture types ─────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct Fixture {
    sql_file: String,
    db_name: String,

    #[serde(default = "default_secret_name")]
    secret_name: String,

    #[serde(default = "default_pod_name")]
    pod_name: String,

    #[serde(default = "default_password_key")]
    password_key: String,

    #[serde(default = "default_username")]
    username: String,

    #[serde(default = "default_port")]
    port: u16,
}

#[derive(Debug, Deserialize)]
struct FixturesFile {
    fixture: Vec<Fixture>,
}

fn default_secret_name() -> String { "pg-postgresql".to_string() }
fn default_pod_name()    -> String { "pg-postgresql-0".to_string() }
fn default_password_key() -> String { "postgres-password".to_string() }
fn default_username()    -> String { "postgres".to_string() }
fn default_port()        -> u16    { 5432 }

// ── Discovery ─────────────────────────────────────────────────────────────────

/// Find all fixtures.toml files directly under platform/<cluster>/ directories.
/// Only searches one level deep — platform/mcs/fixtures.toml,
/// platform/artifactory/fixtures.toml, etc.
fn discover_fixture_files(platform_dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();

    let entries = match fs::read_dir(platform_dir) {
        Ok(e) => e,
        Err(_) => return found,
    };

    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        if path.is_dir() {
            let candidate = path.join("fixtures.toml");
            if candidate.is_file() {
                found.push(candidate);
            }
        }
    }

    found.sort();
    found
}

// ── Secret fetching ───────────────────────────────────────────────────────────

/// Fetch a base64-encoded value from a Kubernetes secret and decode it in Rust.
/// Never shells out to `base64 -d`.
fn fetch_secret_value(
    secret_name: &str,
    key: &str,
    env_vars: &HashMap<String, String>,
) -> anyhow::Result<String> {
    let jsonpath = format!("{{.data.{}}}", key);

    let mut cmd = Command::new("kubectl");
    cmd.args(["get", "secret", secret_name, "-o", &format!("jsonpath={}", jsonpath)]);
    for (k, v) in env_vars {
        cmd.env(k, v);
    }

    let output = cmd.output()
        .map_err(|e| anyhow::anyhow!("Failed to run kubectl get secret: {}", e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!(
            "kubectl get secret '{}' failed (exit {:?}): {}",
            secret_name,
            output.status.code(),
            stderr.trim()
        );
    }

    let b64 = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if b64.is_empty() {
        anyhow::bail!(
            "Secret '{}' has no key '{}' or value is empty",
            secret_name, key
        );
    }

    let decoded = BASE64.decode(b64.as_bytes())
        .map_err(|e| anyhow::anyhow!("Failed to base64-decode secret value: {}", e))?;

    Ok(String::from_utf8_lossy(&decoded).trim().to_string())
}

// ── kubectl exec + stdin ──────────────────────────────────────────────────────

/// Stream SQL content into `kubectl exec -i <pod> -- psql ...` via stdin.
/// PGPASSWORD is passed as an env var on the psql process inside the pod —
/// never appears in process args where `ps aux` could expose it.
fn kubectl_exec_psql(
    pod_name: &str,
    username: &str,
    password: &str,
    db_name: &str,
    port: u16,
    sql_content: &str,
    env_vars: &HashMap<String, String>,
    label: &str,
) -> anyhow::Result<bool> {
    let psql_cmd = format!(
        "env PGPASSWORD={} psql -U {} -d {} -p {}",
        password, username, db_name, port
    );

    let mut cmd = Command::new("kubectl");
    cmd.args([
        "exec", "-i", pod_name,
        "--",
        "sh", "-c", &psql_cmd,
    ]);
    cmd.stdin(Stdio::piped());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    for (k, v) in env_vars {
        cmd.env(k, v);
    }

    let mut child = cmd.spawn()
        .map_err(|e| anyhow::anyhow!("Failed to spawn kubectl exec: {}", e))?;

    if let Some(stdin) = child.stdin.take() {
        let mut stdin = stdin;
        stdin.write_all(sql_content.as_bytes())
            .map_err(|e| anyhow::anyhow!("Failed to write SQL to kubectl stdin: {}", e))?;
        // stdin closes here, signalling EOF to psql
    }

    let output = child.wait_with_output()
        .map_err(|e| anyhow::anyhow!("kubectl exec did not exit cleanly: {}", e))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    if output.status.success() {
        for line in stdout.lines() {
            println!("    {}", line);
        }
        Ok(true)
    } else {
        eprintln!(
            "  ✗ {} — psql failed (exit {:?})",
            label, output.status.code()
        );
        for line in stderr.lines() {
            eprintln!("    {}", line);
        }
        Ok(false)
    }
}

// ── Public entry point ────────────────────────────────────────────────────────
fn kubectl_recreate_db(
    pod_name: &str,
    username: &str,
    password: &str,
    db_name: &str,
    port: u16,
    env_vars: &HashMap<String, String>,
) -> anyhow::Result<()> {
    let recreate_cmd = format!(
        "env PGPASSWORD={pw} psql -U {user} -p {port} -d postgres -c \
        \"SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE datname = '{db}' AND pid <> pg_backend_pid();\" && \
        env PGPASSWORD={pw} dropdb -U {user} -p {port} --if-exists {db} && \
        env PGPASSWORD={pw} createdb -U {user} -p {port} {db}",
        pw = password,
        user = username,
        port = port,
        db = db_name,
    );

    let mut cmd = Command::new("kubectl");
    cmd.args(["exec", "-i", pod_name, "--", "sh", "-c", &recreate_cmd]);
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    for (k, v) in env_vars {
        cmd.env(k, v);
    }

    let output = cmd.output()
        .map_err(|e| anyhow::anyhow!("Failed to run recreate db: {}", e))?;

    let stderr = String::from_utf8_lossy(&output.stderr);

    if !output.status.success() {
        anyhow::bail!("recreate db failed (exit {:?}): {}", output.status.code(), stderr.trim());
    }

    println!("  ✓ database '{}' dropped and recreated", db_name);
    Ok(())
}

pub fn run_load_fixtures() -> anyhow::Result<()> {
    let platform_dir = Path::new("platform");
    if !platform_dir.exists() {
        anyhow::bail!("platform/ directory not found");
    }

    // ── 1. discover all fixtures.toml files ───────────────────────────────────
    let fixture_files = discover_fixture_files(platform_dir);

    if fixture_files.is_empty() {
        println!("No fixtures.toml files found under platform/");
        return Ok(());
    }

    println!("── Load Fixtures ────────────────────────────────────");
    println!("  Found {} fixtures.toml file(s)", fixture_files.len());

    let platform_canonical = platform_dir.canonicalize()?;

    let mut loaded:  Vec<String> = Vec::new();
    let mut failed:  Vec<String> = Vec::new();

    // ── 2. process each fixtures.toml ─────────────────────────────────────────
    for fixtures_path in &fixture_files {
        let cluster_dir = fixtures_path.parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| platform_dir.to_path_buf());

        let cluster_label = cluster_dir
            .strip_prefix(platform_dir)
            .unwrap_or(&cluster_dir)
            .display()
            .to_string();

        println!("\n── {} ──────────────────────────────────────────", cluster_label);

        // resolve .envrc for this cluster directory, bounded by platform/
        let env_vars = match find_envrc_bounded(&cluster_dir, &platform_canonical) {
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

        // parse fixtures.toml
        let toml_content = fs::read_to_string(fixtures_path)
            .map_err(|e| anyhow::anyhow!("Cannot read {}: {}", fixtures_path.display(), e))?;

        let fixtures_file: FixturesFile = toml::from_str(&toml_content)
            .map_err(|e| anyhow::anyhow!("Failed to parse {}: {}", fixtures_path.display(), e))?;

        if fixtures_file.fixture.is_empty() {
            println!("  ⚠ No fixtures defined, skipping");
            continue;
        }

        // ── 3. process each fixture entry ─────────────────────────────────────
        for fixture in &fixtures_file.fixture {
            // sql_file is relative to the fixtures.toml location
            let sql_path = cluster_dir.join(&fixture.sql_file);
            let label = format!("{}/{} → {}", cluster_label, fixture.sql_file, fixture.db_name);

            println!("\n  loading {} ...", label);

            // read SQL file
            let sql_content = match fs::read_to_string(&sql_path) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("  ✗ Cannot read SQL file '{}': {}", sql_path.display(), e);
                    failed.push(label);
                    continue;
                }
            };

            // fetch password from secret — decoded in Rust, never in shell args
            let password = match fetch_secret_value(
                &fixture.secret_name,
                &fixture.password_key,
                &env_vars,
            ) {
                Ok(p) => p,
                Err(e) => {
                    eprintln!("  ✗ {}: {}", label, e);
                    failed.push(label);
                    continue;
                }
            };

            // drop, recreate, then load
            println!("  recreating database '{}' ...", fixture.db_name);
            if let Err(e) = kubectl_recreate_db(
                &fixture.pod_name,
                &fixture.username,
                &password,
                &fixture.db_name,
                fixture.port,
                &env_vars,
            ) {
                eprintln!("  ✗ {}: {}", label, e);
                failed.push(label);
                continue;
            }
            println!("  ✓ database '{}' ready", fixture.db_name);

            // stream SQL into pod via stdin
            match kubectl_exec_psql(
                &fixture.pod_name,
                &fixture.username,
                &password,
                &fixture.db_name,
                fixture.port,
                &sql_content,
                &env_vars,
                &label,
            ) {
                Ok(true) => {
                    println!("  ✓ {}", label);
                    loaded.push(label);
                }
                Ok(false) => {
                    failed.push(label);
                }
                Err(e) => {
                    eprintln!("  ✗ {} — {}", label, e);
                    failed.push(label);
                }
            }
        }
    }

    // ── 4. summary ────────────────────────────────────────────────────────────
    println!("\n── Summary ──────────────────────────────────────────");

    if !loaded.is_empty() {
        println!("  ✓ Loaded ({}):", loaded.len());
        for r in &loaded { println!("      {}", r); }
    }
    if !failed.is_empty() {
        println!("  ✗ Failed ({}):", failed.len());
        for r in &failed { println!("      {}", r); }
    }
    if loaded.is_empty() && failed.is_empty() {
        println!("  nothing to load");
    }

    println!();

    if !failed.is_empty() {
        anyhow::bail!("load-fixtures completed with {} error(s)", failed.len());
    }

    Ok(())
}