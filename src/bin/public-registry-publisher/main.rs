mod common;
mod npm;
mod pypi;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "public-registry-publisher")]
#[command(about = "One-shot sync of approved packages from an internal index to a public registry")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Sync sdists/wheels from a pypiserver PACKAGES_DIR to PyPI (or another
    /// Warehouse-compatible index).
    PushPy(PushPyArgs),
    /// Sync tarballs from a verdaccio STORAGE_DIR to the npm registry.
    PushNode(PushNodeArgs),
}

#[derive(Parser)]
struct PushPyArgs {
    #[arg(long, env = "SYNC_PACKAGES")]
    sync_packages: String,

    #[arg(long, env = "PACKAGES_DIR", default_value = "/data/packages")]
    packages_dir: PathBuf,

    #[arg(long, env = "REPOSITORY_URL", default_value = "https://upload.pypi.org/legacy/")]
    repository_url: String,

    #[arg(long, env = "TWINE_USERNAME", default_value = "__token__")]
    username: String,

    #[arg(long, env = "TWINE_PASSWORD", required_unless_present = "dry_run")]
    password: Option<String>,

    #[arg(long, env = "DRY_RUN", default_value_t = false)]
    dry_run: bool,
}

#[derive(Parser)]
struct PushNodeArgs {
    #[arg(long, env = "SYNC_PACKAGES")]
    sync_packages: String,

    #[arg(long, env = "STORAGE_DIR", default_value = "/verdaccio/storage")]
    storage_dir: PathBuf,

    #[arg(long, env = "NPM_REGISTRY", default_value = "https://registry.npmjs.org")]
    registry: String,

    #[arg(long, env = "NPM_TOKEN", required_unless_present = "dry_run")]
    token: Option<String>,

    #[arg(long, env = "DRY_RUN", default_value_t = false)]
    dry_run: bool,
}

fn log(msg: impl AsRef<str>) {
    let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ");
    println!("[{}] {}", now, msg.as_ref());
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let result = match cli.command {
        Commands::PushPy(args) => run_push_py(args).await,
        Commands::PushNode(args) => run_push_node(args).await,
    };
    if let Err(e) = &result {
        log(format!("ERROR: {:#}", e));
    }
    result
}

async fn run_push_py(args: PushPyArgs) -> Result<()> {
    let packages = common::parse_package_list(&args.sync_packages);
    if packages.is_empty() {
        anyhow::bail!("SYNC_PACKAGES resolved to an empty list, refusing to start.");
    }

    log(format!(
        "public-registry-publisher push-py starting. packages={:?} dir={} dry_run={}",
        packages,
        args.packages_dir.display(),
        args.dry_run
    ));

    let client = reqwest::Client::builder()
        .build()
        .context("building HTTP client")?;

    let mut had_error = false;
    let mut found_any = false;

    for pkg in &packages {
        let matches = common::find_py_matches(&args.packages_dir, pkg)?;
        if matches.is_empty() {
            log(format!("No files found for package '{}', skipping.", pkg));
            continue;
        }

        for path in matches {
            found_any = true;
            let filename = path.display().to_string();

            let dist = match pypi::read_dist_file(&path) {
                Ok(d) => d,
                Err(e) => {
                    log(format!("ERROR: failed to parse {}: {:#}", filename, e));
                    had_error = true;
                    continue;
                }
            };

            if args.dry_run {
                log(format!("DRY_RUN: would upload {}", filename));
                continue;
            }

            let password = args.password.as_deref().unwrap();
            log(format!("Uploading {} for package '{}'.", filename, pkg));
            match pypi::upload(&client, &args.repository_url, &args.username, password, &dist).await {
                Ok(true) => log(format!("Uploaded {} successfully.", filename)),
                Ok(false) => log(format!("{} already exists on index, skipping.", filename)),
                Err(e) => {
                    log(format!("ERROR: {:#}", e));
                    had_error = true;
                }
            }
        }
    }

    if !found_any {
        log("Nothing to publish this run.");
    }

    if had_error {
        anyhow::bail!("one or more uploads failed, see log above");
    }
    Ok(())
}

async fn run_push_node(args: PushNodeArgs) -> Result<()> {
    let packages = common::parse_package_list(&args.sync_packages);
    if packages.is_empty() {
        anyhow::bail!("SYNC_PACKAGES resolved to an empty list, refusing to start.");
    }

    log(format!(
        "public-registry-publisher push-node starting. packages={:?} dir={} dry_run={}",
        packages,
        args.storage_dir.display(),
        args.dry_run
    ));

    let client = reqwest::Client::builder()
        .build()
        .context("building HTTP client")?;

    let mut had_error = false;
    let mut found_any = false;

    for pkg in &packages {
        let pkg_dir = args.storage_dir.join(pkg);
        if !pkg_dir.is_dir() {
            log(format!(
                "No storage directory found for '{}' at {}, skipping.",
                pkg,
                pkg_dir.display()
            ));
            continue;
        }

        let mut tarballs: Vec<PathBuf> = std::fs::read_dir(&pkg_dir)
            .with_context(|| format!("reading {}", pkg_dir.display()))?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("tgz"))
            .collect();
        tarballs.sort();

        if tarballs.is_empty() {
            log(format!("No .tgz files found for '{}', skipping.", pkg));
            continue;
        }

        for tgz in tarballs {
            found_any = true;
            let filename = tgz.display().to_string();

            let tarball = match npm::read_tarball(&tgz) {
                Ok(t) => t,
                Err(e) => {
                    log(format!("WARN: could not read {}: {:#}", filename, e));
                    continue;
                }
            };
            let version = match tarball.package_json.get("version").and_then(|v| v.as_str()) {
                Some(v) => v.to_string(),
                None => {
                    log(format!(
                        "WARN: could not read version from {}, skipping.",
                        filename
                    ));
                    continue;
                }
            };

            if args.dry_run {
                log(format!(
                    "DRY_RUN: would check/publish {}@{} from {}",
                    pkg, version, filename
                ));
                continue;
            }

            let token = args.token.as_deref().unwrap();

            match npm::version_exists(&client, &args.registry, Some(token), pkg, &version).await {
                Ok(true) => {
                    log(format!("{}@{} already published, skipping.", pkg, version));
                    continue;
                }
                Ok(false) => {}
                Err(e) => {
                    log(format!("ERROR: {:#}", e));
                    had_error = true;
                    continue;
                }
            }

            log(format!("Publishing {}@{} from {}.", pkg, version, filename));
            match npm::publish(&client, &args.registry, token, pkg, &version, &tarball).await {
                Ok(()) => log(format!("Published {}@{} successfully.", pkg, version)),
                Err(e) => {
                    log(format!("ERROR: {:#}", e));
                    had_error = true;
                }
            }
        }
    }

    if !found_any {
        log("Nothing to publish this run.");
    }

    if had_error {
        anyhow::bail!("one or more publishes failed, see log above");
    }
    Ok(())
}