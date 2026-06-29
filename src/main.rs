use std::path::Path;

use clap::{Parser, Subcommand};
use ginger_shared_rs::utils::get_token_from_file_storage;
use portalInstaller::install_or_update_portal;
use IAMService::apis::{
    configuration::Configuration as IAMConfiguration, default_api::identity_validate_api_token,
};
use IAMService::get_configuration as get_iam_configuration;
use MetadataService::{
    apis::configuration::Configuration as MetadataConfiguration,
    get_configuration as get_metadata_configuration,
};
use tokio::main;
mod portalInstaller;
mod start;
mod wamp_client;
mod plan;
mod run_dry_run;
mod rollout;
mod load_fixtures;
mod verbose_replace;
mod install_helm_charts;
mod rpc;
mod autoclean;
mod upload;
mod install_tekton_crd;

#[derive(Subcommand, Debug)]
enum Commands {
    Plan,
    /// Run a dry-run of the plan command
    DryRun,
    /// Rollout changes
    Rollout,
    /// installs a portal , creates an entry in the application table in IAM db, this should be run in the FE app repo
    InstallOrUpdatePortal,
    /// deploy by applying in order to a k8 cluster , should take a kubeconfig as an argument , this should also make sure that the DB migrations are run
    Deploy,
    /// Start in daemon mode
    Start {
        /// Unique device identifier
        #[arg(long)]
        device_id: String,
        /// Comma-separated list of capabilities this device supports
        /// (e.g. "osx,osxarm64,osxamd64"). Defaults to "unix" if omitted.
        #[arg(long)]
        capabilities: Option<String>,
    },
    /// Load SQL fixtures into database pods
    LoadFixtures,
    /// Replace verbose
    VerboseReplace {
        /// Input file path
        #[arg(short, long)]
        input: String,
        /// Output file path (optional, defaults to stdout)
        #[arg(short, long)]
        output: Option<String>,
    },
    InstallHelmCharts,
    Rpc {
        /// Path to a .envrc-style file (export NAME=VALUE lines) to load as env vars
        #[arg(long)]
        envrc: String,
        /// Path to the script to execute
        #[arg(long)]
        script: String,
        /// Path to an optional cleanup script to run after the main script
        #[arg(long)]
        cleanup: Option<String>,
        /// Device capability to target (e.g. "unix")
        #[arg(long, default_value = "unix")]
        capability: String,
    },
    InstallTektonCrd {
        /// Controller image to deploy (default: gingersociety/remote-task-controller:latest)
        #[arg(long)]
        image: Option<String>,
        /// Default executor URL to bake into the controller Deployment's env
        #[arg(long)]
        executor_url: Option<String>,
        #[arg(long)]
        runner_image: Option<String>,
    },
    /// Upload a file to the bucket service
    Upload {
        /// Bucket path (may contain slashes, e.g. "videos/2024")
        bucket_path: String,
        /// Local file to upload
        file: String,
        /// Overwrite if file already exists
        #[arg(long, default_value_t = false)]
        overwrite: bool,
    },
}

#[derive(Parser, Debug)]
#[command(name = "ginger-infra")]
#[command(about = "A tool which wraps various commands from kubectl and helm , used for managing environments", long_about = None)]
#[command(version, long_about = None)]
struct Args {
    /// name of the command to run
    #[command(subcommand)]
    command: Commands,
}


async fn check_session_gurad(
    cli: Args,
    config_path: &Path,
    iam_config: &IAMConfiguration,
    metadata_config: &MetadataConfiguration,
    package_path: &Path,
    releaser_path: &Path,
    token: String,
) {
    match cli.command {
        Commands::InstallTektonCrd { image, executor_url, runner_image } => {
            if let Err(e) = install_tekton_crd::run_install_tekton_crd(
                image.as_deref(),
                executor_url.as_deref(),
                runner_image.as_deref(),
            ) {
                eprintln!("install-tekton-crd failed: {e}");
                std::process::exit(1);
            }
        }
        Commands::Rpc { envrc, script, cleanup, capability } => {
            rpc::run_rpc(&envrc, &script, cleanup.as_deref(), &capability).await;
        }
        Commands::Plan => {
            if let Err(e) = plan::run_plan() {
                eprintln!("plan failed: {e}");
                std::process::exit(1);
            }
        }
        Commands::LoadFixtures => {
            if let Err(e) = load_fixtures::run_load_fixtures() {
                eprintln!("load-fixtures failed: {e}");
                std::process::exit(1);
            }
        }
        Commands::Rollout => {
            if let Err(e) = rollout::run_rollout() {
                eprintln!("plan failed: {e}");
                std::process::exit(1);
            }
        }
        Commands::VerboseReplace { input, output } => {
            let input_path = Path::new(&input);
            let output_path = output.as_deref().map(Path::new);
            if let Err(e) = verbose_replace::run_verbose_replace(input_path, output_path) {
                eprintln!("verbose-replace failed: {e}");
                std::process::exit(1);
            }
        }
        Commands::DryRun => {
            if let Err(e) = run_dry_run::run_dry_run() {
                eprintln!("dry run failed: {e}");
                std::process::exit(1);
            }
        }
        Commands::InstallOrUpdatePortal => {
            install_or_update_portal(config_path, &iam_config, releaser_path, package_path).await
        }
        Commands::Deploy => todo!(),
        Commands::Start { device_id, capabilities } => {
            let capabilities: Vec<String> = capabilities
                .as_deref()
                .map(|csv| {
                    csv.split(',')
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect::<Vec<_>>()
                })
                .filter(|v| !v.is_empty())
                .unwrap_or_else(|| vec!["unix".to_string()]);

            if capabilities.iter().any(|c| c == "autoclean-docker") {
                tokio::spawn(async {
                    autoclean::start_autoclean_scheduler().await;
                });
            }

            match identity_validate_api_token(&iam_config).await {
                Ok(response) => {
                    start::main(token.clone(), response, &metadata_config, device_id, capabilities).await;
                }
                Err(error) => {
                    println!("Token validation failed: {:?}", error);
                    std::process::exit(1);
                }
            }
        }
        Commands::InstallHelmCharts => {
            if let Err(e) = install_helm_charts::run_install_helm_charts() {
                eprintln!("install-helm-charts failed: {e}");
                std::process::exit(1);
            }
        }
        Commands::Upload { bucket_path, file, overwrite } => {
            upload::run_upload(&bucket_path, &file, overwrite).await;
        }
    }
}

#[main]
async fn main() {
    let args = Args::parse();
    let token = get_token_from_file_storage();
    let metadata_config: MetadataConfiguration = get_metadata_configuration(Some(token.clone()));
    let iam_config: IAMConfiguration = get_iam_configuration(Some(token.clone()));
    let service_config_path = Path::new("services.toml");
    let package_path = Path::new("metadata.toml");
    let releaser_path = Path::new("releaser.toml");

    check_session_gurad(
        args,
        service_config_path,
        &iam_config,
        &metadata_config,
        package_path,
        releaser_path,
        token,
    ).await;
}