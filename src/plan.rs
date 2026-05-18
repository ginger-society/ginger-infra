use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use tera::{Context, Tera};
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

// ── Values validation ─────────────────────────────────────────────────────────

/// Recursively collect all dot-notation leaf keys from a JSON object
/// { "iam": { "jwt_secret": "x" } } → ["iam.jwt_secret"]
fn collect_keys(value: &serde_json::Value, prefix: &str, keys: &mut HashSet<String>) {
    match value {
        serde_json::Value::Object(map) => {
            for (k, v) in map {
                let full_key = if prefix.is_empty() {
                    k.clone()
                } else {
                    format!("{}.{}", prefix, k)
                };
                collect_keys(v, &full_key, keys);
            }
        }
        _ => {
            keys.insert(prefix.to_string());
        }
    }
}

fn check_unfilled_values(value: &serde_json::Value, prefix: &str, unfilled: &mut Vec<String>) {
    match value {
        serde_json::Value::Object(map) => {
            for (k, v) in map {
                let full_key = if prefix.is_empty() {
                    k.clone()
                } else {
                    format!("{}.{}", prefix, k)
                };
                check_unfilled_values(v, &full_key, unfilled);
            }
        }
        serde_json::Value::String(s) => {
            if s == "CHANGE_ME" || s.is_empty() {
                unfilled.push(prefix.to_string());
            }
        }
        _ => {}
    }
}

fn validate_values(values_path: &Path, backup_path: &Path) -> anyhow::Result<serde_json::Value> {
    // 1. load backup (the expected shape / schema)
    if !backup_path.exists() {
        anyhow::bail!(
            "values.json.backup not found at {}.\n\
             This file defines the expected shape of values.json and must be committed.",
            backup_path.display()
        );
    }
    let backup: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(backup_path)?)
            .map_err(|e| anyhow::anyhow!("Failed to parse values.json.backup: {e}"))?;

    // 2. load values.json (the actual secrets — gitignored)
    if !values_path.exists() {
        anyhow::bail!(
            "values.json not found at {}.\n\
             Copy values.json.backup → values.json and fill in real values.\n\
             values.json is gitignored and must never be committed.",
            values_path.display()
        );
    }
    let values: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(values_path)?)
            .map_err(|e| anyhow::anyhow!("Failed to parse values.json: {e}"))?;

    // 3. collect keys from both
    let mut backup_keys: HashSet<String> = HashSet::new();
    let mut values_keys: HashSet<String> = HashSet::new();
    collect_keys(&backup, "", &mut backup_keys);
    collect_keys(&values, "", &mut values_keys);

    // 4. diff
    let mut missing: Vec<_> = backup_keys.difference(&values_keys).cloned().collect();
    let mut extra: Vec<_> = values_keys.difference(&backup_keys).cloned().collect();
    missing.sort();
    extra.sort();

    let mut has_errors = false;

    if !missing.is_empty() {
        has_errors = true;
        eprintln!("\n✗ Keys in values.json.backup MISSING from values.json:");
        for key in &missing {
            eprintln!("    - {}", key);
        }
        eprintln!(
            "\n  The IAC was updated with new keys.\n  \
             Add them to values.json before running plan."
        );
    }

    if !extra.is_empty() {
        has_errors = true;
        eprintln!("\n✗ Keys in values.json NOT present in values.json.backup:");
        for key in &extra {
            eprintln!("    - {}", key);
        }
        eprintln!(
            "\n  Either remove these from values.json\n  \
             or add them to values.json.backup and commit it."
        );
    }

    if has_errors {
        anyhow::bail!(
            "\nvalues.json and values.json.backup are out of sync.\n\
             Fix the mismatches above then re-run `ginger-infra plan`."
        );
    }

    // 5. check for unfilled placeholders
    let mut unfilled: Vec<String> = Vec::new();
    check_unfilled_values(&values, "", &mut unfilled);
    unfilled.sort();

    if !unfilled.is_empty() {
        eprintln!("\n✗ Unfilled placeholder values in values.json:");
        for key in &unfilled {
            eprintln!("    - {}", key);
        }
        anyhow::bail!(
            "\nFill in all placeholder values in values.json before running plan."
        );
    }

    println!(
        "✓ values.json validated against values.json.backup ({} keys)",
        backup_keys.len()
    );

    Ok(values)
}

// ── Tera context builder ──────────────────────────────────────────────────────

fn build_tera_context(snapshot: &Snapshot, values: serde_json::Value) -> Context {
    let mut ctx = Context::new();

    let services_map: serde_json::Map<String, serde_json::Value> = snapshot
        .services
        .iter()
        .map(|s| (s.identifier.clone(), serde_json::Value::String(s.version.clone())))
        .collect();
    ctx.insert("services", &services_map);

    let packages_map: serde_json::Map<String, serde_json::Value> = snapshot
        .packages
        .iter()
        .map(|p| (p.identifier.clone(), serde_json::Value::String(p.version.clone())))
        .collect();
    ctx.insert("packages", &packages_map);

    let databases_map: serde_json::Map<String, serde_json::Value> = snapshot
        .databases
        .iter()
        .map(|d| (d.name.clone(), serde_json::Value::String(d.version.clone())))
        .collect();
    ctx.insert("databases", &databases_map);

    ctx.insert("services_list", &snapshot.services);
    ctx.insert("packages_list", &snapshot.packages);
    ctx.insert("databases_list", &snapshot.databases);

    // available in templates as {{ values.iam.jwt_secret }} etc.
    ctx.insert("values", &values);

    ctx
}

// ── File discovery ────────────────────────────────────────────────────────────

#[derive(Debug)]
pub struct PlatformFile {
    pub absolute_path: PathBuf,
    pub relative_path: PathBuf,
    pub file_type: PlatformFileType,
}

#[derive(Debug, PartialEq)]
pub enum PlatformFileType {
    Yaml,
    SecretsReadme,
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

        let file_type = if file_name == "secrets.README.md" {
            PlatformFileType::SecretsReadme
        } else if file_name.ends_with(".yaml") || file_name.ends_with(".yml") {
            PlatformFileType::Yaml
        } else {
            continue;
        };

        let relative_path = path
            .strip_prefix(platform_dir)
            .unwrap_or(&path)
            .to_path_buf();

        files.push(PlatformFile {
            absolute_path: path,
            relative_path,
            file_type,
        });
    }

    files.sort_by(|a, b| a.relative_path.cmp(&b.relative_path));
    files
}

// ── Build output ──────────────────────────────────────────────────────────────

fn copy_to_build(
    files: &[PlatformFile],
    build_dir: &Path,
    tera_ctx: &Context,
) -> anyhow::Result<()> {
    if build_dir.exists() {
        fs::remove_dir_all(build_dir)?;
    }

    for file in files {
        let dest = build_dir.join(&file.relative_path);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }

        let content = fs::read_to_string(&file.absolute_path)?;

        let rendered = if content.contains("{{") || content.contains("{%") {
            let mut tera = Tera::default();
            let template_name = file.relative_path.to_string_lossy().to_string();
            tera.add_raw_template(&template_name, &content)
                .map_err(|e| anyhow::anyhow!("Failed to parse '{}': {e}", template_name))?;
            tera.render(&template_name, tera_ctx)
                .map_err(|e| anyhow::anyhow!("Failed to render '{}': {e}", template_name))?
        } else {
            content
        };

        fs::write(&dest, rendered)?;
    }

    Ok(())
}

// ── Public entry point ────────────────────────────────────────────────────────

pub fn run_plan() -> anyhow::Result<()> {
    // 1. snapshot
    let snapshot_path = Path::new("snapshot.json");
    if !snapshot_path.exists() {
        anyhow::bail!("snapshot.json not found in current directory");
    }
    let snapshot: Snapshot = serde_json::from_str(&fs::read_to_string(snapshot_path)?)
        .map_err(|e| anyhow::anyhow!("Failed to parse snapshot.json: {e}"))?;

    println!("✓ Loaded snapshot.json");
    println!(
        "  services: {}  packages: {}  databases: {}",
        snapshot.services.len(),
        snapshot.packages.len(),
        snapshot.databases.len()
    );

    // 2. validate values
    let values = validate_values(Path::new("values.json"), Path::new("values.json.backup"))?;

    // 3. tera context
    let tera_ctx = build_tera_context(&snapshot, values);
    println!("✓ Built Tera context");

    // 4. discover
    let platform_dir = Path::new("platform");
    if !platform_dir.exists() {
        anyhow::bail!("platform/ directory not found");
    }
    let files = discover_platform_files(platform_dir);

    let yaml_files: Vec<_> = files.iter().filter(|f| f.file_type == PlatformFileType::Yaml).collect();
    let secret_files: Vec<_> = files.iter().filter(|f| f.file_type == PlatformFileType::SecretsReadme).collect();

    println!("\n── YAML / YML ({}) ──────────────────────────────────", yaml_files.len());
    for f in &yaml_files {
        println!("  platform/{}", f.relative_path.display());
    }

    println!("\n── secrets.README.md ({}) ───────────────────────────", secret_files.len());
    for f in &secret_files {
        println!("  platform/{}", f.relative_path.display());
    }

    // 5. render to build/
    let build_dir = Path::new("build");
    copy_to_build(&files, build_dir, &tera_ctx)?;

    println!("\n✓ Rendered {} files → build/", files.len());
    println!("Next: review build/ then run `ginger-infra deploy`");

    Ok(())
}