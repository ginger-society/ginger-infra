use std::fs;
use std::path::Path;

use crate::plan::{load_and_resolve_values, render_template, Snapshot};

pub fn run_verbose_replace(input_path: &Path, output_path: Option<&Path>) -> anyhow::Result<()> {
    // load snapshot
    let snapshot_path = Path::new("snapshot.json");
    if !snapshot_path.exists() {
        anyhow::bail!("snapshot.json not found in current directory");
    }
    let snapshot: Snapshot = serde_json::from_str(&fs::read_to_string(snapshot_path)?)
        .map_err(|e| anyhow::anyhow!("Failed to parse snapshot.json: {e}"))?;

    // load + resolve values
    let resolved = load_and_resolve_values(
        Path::new("values.json"),
        Path::new("values.json.backup"),
        Path::new("values.memory.json"),
    )?;

    // read input file
    if !input_path.exists() {
        anyhow::bail!("Input file not found: {}", input_path.display());
    }
    let content = fs::read_to_string(input_path)
        .map_err(|e| anyhow::anyhow!("Cannot read '{}': {e}", input_path.display()))?;

    // render
    let template_name = input_path.to_string_lossy().to_string();
    let rendered = render_template(&content, &template_name, &resolved, &snapshot)?;

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