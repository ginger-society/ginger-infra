use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::run_dry_run::{collect_yamls, find_envrc_bounded, parse_envrc};

// ── Vault loading ─────────────────────────────────────────────────────────────

/// Load vault.json from cwd. Keys are vault secret names, values are plaintext.
fn load_vault(vault_path: &Path) -> anyhow::Result<HashMap<String, String>> {
    if !vault_path.exists() {
        anyhow::bail!(
            "vault.json not found in current directory.\n\
             It must be a JSON object mapping secret keys to their plaintext values."
        );
    }

    let raw = fs::read_to_string(vault_path)?;
    let map: HashMap<String, String> = serde_json::from_str(&raw)
        .map_err(|e| anyhow::anyhow!("Failed to parse vault.json: {e}"))?;

    println!("✓ vault.json loaded ({} secrets)", map.len());
    Ok(map)
}

// ── Vault substitution ────────────────────────────────────────────────────────

/// Replace all `vault(KEY)` placeholders in rendered YAML content with
/// the plaintext values from vault.json.  Never writes anything to disk.
fn substitute_vault(content: &str, vault: &HashMap<String, String>, label: &str) -> anyhow::Result<String> {
    // We need a simple but correct parser: find vault(...) and replace.
    // Using a hand-rolled approach to avoid pulling in another regex dep
    // (regex is already a dep in plan.rs so it is available in the crate).
    use regex::Regex;

    let re = Regex::new(r"vault\(([^)]+)\)").unwrap();

    let mut errors: Vec<String> = Vec::new();

    let result = re.replace_all(content, |caps: &regex::Captures| {
        let key = caps.get(1).map(|m| m.as_str().trim()).unwrap_or("");
        match vault.get(key) {
            Some(v) => v.clone(),
            None => {
                errors.push(format!("vault key '{}' not found in vault.json", key));
                // return placeholder so replacement is still a valid string
                format!("vault({})", key)
            }
        }
    }).to_string();

    if !errors.is_empty() {
        anyhow::bail!(
            "'{}' has unresolved vault references:\n{}",
            label,
            errors.iter().map(|e| format!("  - {}", e)).collect::<Vec<_>>().join("\n")
        );
    }

    Ok(result)
}

// ── kubectl apply via stdin ───────────────────────────────────────────────────

/// Pipe `content` into `kubectl apply -f -` with the given env vars.
/// Nothing is written to disk.
fn kubectl_apply_stdin(
    content: &str,
    env_vars: &HashMap<String, String>,
    label: &str,
) -> anyhow::Result<bool> {
    let mut cmd = Command::new("kubectl");
    cmd.args(["apply", "-f", "-"]);
    cmd.stdin(Stdio::piped());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    for (k, v) in env_vars {
        cmd.env(k, v);
    }

    let mut child = cmd.spawn().map_err(|e| anyhow::anyhow!("Failed to spawn kubectl: {}", e))?;

    // write manifest to kubectl's stdin then close it
    if let Some(stdin) = child.stdin.take() {
        let mut stdin = stdin;
        stdin.write_all(content.as_bytes())
            .map_err(|e| anyhow::anyhow!("Failed to write to kubectl stdin: {}", e))?;
        // stdin drops here, closing the pipe
    }

    let output = child.wait_with_output()
        .map_err(|e| anyhow::anyhow!("kubectl did not exit cleanly: {}", e))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    if output.status.success() {
        for line in stdout.lines() {
            println!("    {}", line);
        }
        Ok(true)
    } else {
        eprintln!("  ✗ {} — kubectl apply failed (exit {:?})", label, output.status.code());
        for line in stderr.lines() {
            eprintln!("    {}", line);
        }
        Ok(false)
    }
}

// ── Public entry point ────────────────────────────────────────────────────────

pub fn run_rollout() -> anyhow::Result<()> {
    // ── 1. load vault.json (never written back, stays in memory) ─────────────
    let vault = load_vault(Path::new("vault.json"))?;

    // ── 2. run plan to populate build/ ───────────────────────────────────────
    println!("\n── Running plan before rollout ──────────────────────");
    crate::plan::run_plan()?;

    // ── 3. collect yaml files from build/ ────────────────────────────────────
    let build_dir = Path::new("build");
    if !build_dir.exists() {
        anyhow::bail!("build/ not found — plan must have failed");
    }

    let mut yaml_files: Vec<PathBuf> = Vec::new();
    collect_yamls(build_dir, &mut yaml_files);
    yaml_files.sort();

    if yaml_files.is_empty() {
        anyhow::bail!("build/ is empty — nothing to apply");
    }

    let build_dir_canonical = build_dir.canonicalize()?;

    println!("\n── kubectl apply ────────────────────────────────────");

    let mut applied: Vec<String> = Vec::new();
    let mut failed:  Vec<String> = Vec::new();

    // ── 4. for each file: vault-substitute in memory, pipe to kubectl ─────────
    for file in &yaml_files {
        let label = file
            .strip_prefix(build_dir)
            .unwrap_or(file)
            .display()
            .to_string();

        // read rendered content (vault placeholders still present)
        let content = match fs::read_to_string(file) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("  ✗ {} — cannot read file: {}", label, e);
                failed.push(label);
                continue;
            }
        };

        // vault substitution entirely in memory
        let final_content = match substitute_vault(&content, &vault, &label) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("  ✗ {}", e);
                failed.push(label);
                continue;
            }
        };

        // resolve .envrc scoped to this file's directory, bounded by build/
        let file_dir = file.parent().map(Path::to_path_buf)
            .unwrap_or_else(|| build_dir.to_path_buf());

        let env_vars = match find_envrc_bounded(&file_dir, &build_dir_canonical) {
            Some(envrc_path) => {
                let envrc_content = match fs::read_to_string(&envrc_path) {
                    Ok(c) => c,
                    Err(e) => {
                        eprintln!("  ✗ {} — cannot read .envrc: {}", label, e);
                        failed.push(label);
                        continue;
                    }
                };
                parse_envrc(&envrc_content)
            }
            None => HashMap::new(),
        };

        print!("  applying {} ... ", label);

        match kubectl_apply_stdin(&final_content, &env_vars, &label) {
            Ok(true) => {
                applied.push(label);
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

    // ── 5. summary ────────────────────────────────────────────────────────────
    println!("\n── Summary ──────────────────────────────────────────");

    if !applied.is_empty() {
        println!("  ✓ Applied ({}):", applied.len());
        for r in &applied {
            println!("      {}", r);
        }
    }

    if !failed.is_empty() {
        println!("  ✗ Failed ({}):", failed.len());
        for r in &failed {
            println!("      {}", r);
        }
    }

    if failed.is_empty() {
        println!("\n  ✓ Rollout complete — all resources applied successfully.");
    }

    println!();

    if !failed.is_empty() {
        anyhow::bail!("Rollout completed with {} error(s) — fix above before retrying", failed.len());
    }

    Ok(())
}