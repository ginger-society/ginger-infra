use std::fs;
use std::path::Path;
use std::process::Command;

/// Runs the full plan pipeline then for each rendered file in build/
/// calls `kubectl diff -f <file>` to show what would change in the cluster.
/// Does NOT apply anything.
pub fn run_dry_run() -> anyhow::Result<()> {
    // 1. run plan first to populate build/
    println!("── Running plan before dry-run ──────────────────────");
    crate::plan::run_plan()?;

    // 2. collect all yaml files from build/
    let build_dir = Path::new("build");
    if !build_dir.exists() {
        anyhow::bail!("build/ not found — plan must have failed");
    }

    let mut yaml_files: Vec<std::path::PathBuf> = Vec::new();
    collect_yamls(build_dir, &mut yaml_files);
    yaml_files.sort();

    if yaml_files.is_empty() {
        anyhow::bail!("build/ is empty — nothing to diff");
    }

    println!("\n── kubectl diff ─────────────────────────────────────");

    let mut any_diff  = false;
    let mut any_new   = false;
    let mut any_error = false;
    let mut new_resources: Vec<String>     = Vec::new();
    let mut changed_resources: Vec<String> = Vec::new();
    let mut error_resources: Vec<String>   = Vec::new();

    for file in &yaml_files {
        let label = file.strip_prefix(build_dir)
            .unwrap_or(file)
            .display()
            .to_string();

        // kubectl diff exit codes:
        //   0 — no diff
        //   1 — diff found (or resource doesn't exist yet)
        //   >1 — real error
        let output = Command::new("kubectl")
            .args(["diff", "-f"])
            .arg(file)
            .output();

        match output {
            Err(e) => {
                eprintln!("  ✗ {} — failed to run kubectl: {}", label, e);
                error_resources.push(label);
                any_error = true;
            }
            Ok(out) => {
                let stdout = String::from_utf8_lossy(&out.stdout);
                let stderr = String::from_utf8_lossy(&out.stderr);

                match out.status.code() {
                    Some(0) => {
                        println!("  ✓ {} — no changes", label);
                    }
                    Some(1) => {
                        // "not found" means new resource, not a diff
                        if stderr.contains("not found") || stderr.contains("NotFound") {
                            println!("  ✦ {} — NEW (will be created)", label);
                            new_resources.push(label);
                            any_new = true;
                        } else {
                            println!("\n  ~ {} — changes:", label);
                            for line in stdout.lines() {
                                if line.starts_with('+') {
                                    println!("    \x1b[32m{}\x1b[0m", line);
                                } else if line.starts_with('-') {
                                    println!("    \x1b[31m{}\x1b[0m", line);
                                } else {
                                    println!("    {}", line);
                                }
                            }
                            changed_resources.push(label);
                            any_diff = true;
                        }
                    }
                    Some(code) => {
                        eprintln!("  ✗ {} — kubectl error (exit {}): {}", label, code, stderr.trim());
                        error_resources.push(label);
                        any_error = true;
                    }
                    None => {
                        eprintln!("  ✗ {} — kubectl killed by signal", label);
                        any_error = true;
                    }
                }
            }
        }
    }

    // ── summary ───────────────────────────────────────────────────────────────
    println!("\n── Summary ──────────────────────────────────────────");

    if !new_resources.is_empty() {
        println!("  ✦ New ({}):", new_resources.len());
        for r in &new_resources { println!("      {}", r); }
    }
    if !changed_resources.is_empty() {
        println!("  ~ Changed ({}):", changed_resources.len());
        for r in &changed_resources { println!("      {}", r); }
    }
    if !error_resources.is_empty() {
        println!("  ✗ Errors ({}):", error_resources.len());
        for r in &error_resources { println!("      {}", r); }
    }
    if !any_diff && !any_new && !any_error {
        println!("  ✓ No changes — cluster is already up to date");
    }

    println!();
    if any_error {
        anyhow::bail!("Dry run completed with errors — fix above before deploying");
    }
    if any_diff || any_new {
        println!("Run `ginger-infra deploy` to apply these changes.");
    }

    Ok(())
}

fn collect_yamls(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.filter_map(|e| e.ok()) {
            let path = entry.path();
            if path.is_dir() {
                collect_yamls(&path, out);
            } else if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
                if ext == "yaml" || ext == "yml" {
                    out.push(path);
                }
            }
        }
    }
}