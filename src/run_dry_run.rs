use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

// ── .envrc discovery & parsing ───────────────────────────────────────────────

// ── .envrc discovery ──────────────────────────────────────────────────────────

/// Walk UP from `start` but STOP at (and never go above) `ceiling`.
/// This prevents escaping the build/ directory and picking up a system .envrc.
fn find_envrc_bounded(start: &Path, ceiling: &Path) -> Option<PathBuf> {
    let ceiling = ceiling.canonicalize().ok()?;
    let mut dir = start.canonicalize().ok()?;

    loop {
        // Never search above the ceiling
        if !dir.starts_with(&ceiling) {
            return None;
        }

        let candidate = dir.join(".envrc");
        if candidate.is_file() {
            return Some(candidate);
        }

        // Stop exactly at ceiling — don't pop further
        if dir == ceiling {
            return None;
        }

        if !dir.pop() {
            return None;
        }
    }
}

/// Parse `export KEY=VALUE` (and bare `KEY=VALUE`) lines from an `.envrc`.
/// Expands `$HOME` and `${HOME}` using the real process environment so that
/// paths like `$HOME/Downloads/kubeconfig/artifactory.yml` resolve correctly.
fn parse_envrc(content: &str) -> HashMap<String, String> {
    let mut vars: HashMap<String, String> = HashMap::new();

    for raw in content.lines() {
        let line = raw.trim();

        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        // strip leading `export ` if present
        let line = line.strip_prefix("export ").unwrap_or(line);

        if let Some((key, value)) = line.split_once('=') {
            let key = key.trim().to_string();

            // strip surrounding quotes
            let value = value.trim();
            let value = value
                .strip_prefix('"')
                .and_then(|v| v.strip_suffix('"'))
                .or_else(|| {
                    value
                        .strip_prefix('\'')
                        .and_then(|v| v.strip_suffix('\''))
                })
                .unwrap_or(value);

            // expand $HOME / ${HOME} using the real environment
            let value = expand_home(value);

            vars.insert(key, value);
        }
    }

    vars
}

/// Replace `$HOME` and `${HOME}` with the value of the `HOME` env var.
fn expand_home(s: &str) -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    s.replace("${HOME}", &home).replace("$HOME", &home)
}

// ── yaml collection (unchanged) ───────────────────────────────────────────────

fn collect_yamls(dir: &Path, out: &mut Vec<PathBuf>) {
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

// ── main entry point ──────────────────────────────────────────────────────────

/// Runs the full plan pipeline then for each rendered file in build/
/// calls `kubectl diff -f <file>` to show what would change in the cluster.
/// Picks up KUBECONFIG (and any other vars) from the nearest `.envrc` walking
/// up from the current working directory.  Does NOT apply anything.
pub fn run_dry_run() -> anyhow::Result<()> {
    // ── 1. locate & load .envrc ───────────────────────────────────────────────
    let cwd = std::env::current_dir()?;

    // ── 2. run plan to populate build/ ────────────────────────────────────────
    println!("\n── Running plan before dry-run ──────────────────────");
    crate::plan::run_plan()?;

    // ── 3. collect yaml files ─────────────────────────────────────────────────
    let build_dir = Path::new("build");
    if !build_dir.exists() {
        anyhow::bail!("build/ not found — plan must have failed");
    }

    let mut yaml_files: Vec<PathBuf> = Vec::new();
    collect_yamls(build_dir, &mut yaml_files);
    yaml_files.sort();

    if yaml_files.is_empty() {
        anyhow::bail!("build/ is empty — nothing to diff");
    }

    println!("\n── kubectl diff ─────────────────────────────────────");

    let mut any_diff = false;
    let mut any_new = false;
    let mut any_error = false;
    let mut new_resources: Vec<String> = Vec::new();
    let mut changed_resources: Vec<String> = Vec::new();
    let mut error_resources: Vec<String> = Vec::new();

    // ── 4. diff each file ─────────────────────────────────────────────────────

    let build_dir_canonical = build_dir.canonicalize()?;

    for file in &yaml_files {
        let label = file
        .strip_prefix(build_dir)
        .unwrap_or(file)
        .display()
        .to_string();

        // Resolve .envrc scoped to this file's directory, bounded by build/
        let file_dir = file.parent().map(Path::to_path_buf)
            .unwrap_or_else(|| build_dir.to_path_buf());

        let env_vars = match find_envrc_bounded(&file_dir, &build_dir_canonical) {
            Some(envrc_path) => {
                let content = fs::read_to_string(&envrc_path)?;
                parse_envrc(&content)
            }
            None => HashMap::new(),
        };

        let mut cmd = Command::new("kubectl");
        cmd.args(["diff", "-f"]).arg(file);
        for (k, v) in &env_vars {
            cmd.env(k, v);
        }

        let output = cmd.output();

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
                        eprintln!(
                            "  ✗ {} — kubectl error (exit {}): {}",
                            label,
                            code,
                            stderr.trim()
                        );
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

    // ── 5. summary ────────────────────────────────────────────────────────────
    println!("\n── Summary ──────────────────────────────────────────");

    if !new_resources.is_empty() {
        println!("  ✦ New ({}):", new_resources.len());
        for r in &new_resources {
            println!("      {}", r);
        }
    }
    if !changed_resources.is_empty() {
        println!("  ~ Changed ({}):", changed_resources.len());
        for r in &changed_resources {
            println!("      {}", r);
        }
    }
    if !error_resources.is_empty() {
        println!("  ✗ Errors ({}):", error_resources.len());
        for r in &error_resources {
            println!("      {}", r);
        }
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