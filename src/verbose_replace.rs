use std::collections::HashMap;
use std::fs;
use std::path::Path;

use crate::plan::{load_and_resolve_values, render_template, resolve_shell_values, Snapshot};
use crate::run_dry_run::{find_envrc_bounded, parse_envrc};

pub fn run_verbose_replace(input_path: &Path, output_path: Option<&Path>) -> anyhow::Result<()> {
    // load snapshot
    let snapshot_path = Path::new("snapshot.json");
    if !snapshot_path.exists() {
        anyhow::bail!("snapshot.json not found in current directory");
    }
    let snapshot: Snapshot = serde_json::from_str(&fs::read_to_string(snapshot_path)?)
        .map_err(|e| anyhow::anyhow!("Failed to parse snapshot.json: {e}"))?;

    // load + resolve values (shell() still deferred at this point)
    let resolved = load_and_resolve_values(
        Path::new("values.json"),
        Path::new("values.json.backup"),
        Path::new("values.memory.json"),
    )?;

    // resolve shell() values scoped to the input file's nearest .envrc,
    // bounded by cwd — mirrors what render_to_build does per platform file
    let platform_dir = Path::new("platform");
    let platform_canonical = if platform_dir.exists() {
        platform_dir.canonicalize()?
    } else {
        std::env::current_dir()?
    };

    let file_dir = input_path
        .canonicalize()
        .unwrap_or_else(|_| input_path.to_path_buf())
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());

    let env_vars = match find_envrc_bounded(&file_dir, &platform_canonical) {
        Some(envrc_path) => {
            println!(
                "  .envrc context: {}",
                envrc_path.display()
            );
            let content = fs::read_to_string(&envrc_path)
                .map_err(|e| anyhow::anyhow!("Cannot read .envrc: {e}"))?;
            parse_envrc(&content)
        }
        None => {
            println!("  ⚠ No .envrc found — shell() will use inherited environment");
            HashMap::new()
        }
    };

    let resolved = resolve_shell_values(&resolved, &env_vars)?;

    // load vault.json and substitute vault() placeholders — same as rollout.rs
    let vault_path = Path::new("vault.json");
    let vault: HashMap<String, String> = if vault_path.exists() {
        let raw = fs::read_to_string(vault_path)?;
        serde_json::from_str(&raw)
            .map_err(|e| anyhow::anyhow!("Failed to parse vault.json: {e}"))?
    } else {
        println!("  ⚠ vault.json not found — vault() placeholders will remain as-is");
        HashMap::new()
    };

    // read input file
    if !input_path.exists() {
        anyhow::bail!("Input file not found: {}", input_path.display());
    }
    let content = fs::read_to_string(input_path)
        .map_err(|e| anyhow::anyhow!("Cannot read '{}': {e}", input_path.display()))?;

    // render template (handles {{KEY}}, services[], packages[], databases[])
    let template_name = input_path.to_string_lossy().to_string();
    let rendered = render_template(&content, &template_name, &resolved, &snapshot)?;

    // substitute vault() placeholders in the rendered output — identical to rollout.rs
    let rendered = substitute_vault(&rendered, &vault, &template_name)?;

    // write or print
    match output_path {
        Some(p) => {
            if let Some(parent) = p.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(p, &rendered)
                .map_err(|e| anyhow::anyhow!("Cannot write '{}': {e}", p.display()))?;
            println!("✓ Written to {}", p.display());
        }
        None => print!("{}", rendered),
    }

    Ok(())
}

// mirrors rollout::substitute_vault — kept local to avoid making that fn pub
fn substitute_vault(
    content: &str,
    vault: &HashMap<String, String>,
    label: &str,
) -> anyhow::Result<String> {
    use regex::Regex;
    let re = Regex::new(r"vault\(([^)]+)\)").unwrap();
    let mut errors: Vec<String> = Vec::new();

    let result = re.replace_all(content, |caps: &regex::Captures| {
        let key = caps.get(1).map(|m| m.as_str().trim()).unwrap_or("");
        match vault.get(key) {
            Some(v) => v.clone(),
            None => {
                errors.push(format!("vault key '{}' not found in vault.json", key));
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