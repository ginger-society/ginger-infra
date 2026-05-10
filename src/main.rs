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
mod heartbeat;

#[derive(Subcommand, Debug)]
enum Commands {
    /// Initialize a infra project
    Init,
    /// gives the option to select an item from the dev portal and generate its deployment , service and update ingress
    Add,
    // after taking a snapshot using ginger-releaser , we can apply it using this - this might be a stop action , but can be called immediately after the releaser command
    ApplySnapshot,
    /// installs a portal , creates an entry in the application table in IAM db, this should be run in the FE app repo
    InstallOrUpdatePortal,
    /// deploy by applying in order to a k8 cluster , should take a kubeconfig as an argument , this should also make sure that the DB migrations are run
    Deploy,
    Start
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
    match identity_validate_api_token(&iam_config).await {
        Ok(response) => {
            match cli.command {
                Commands::Init => {
                    println!("Hello, world!");
                }
                Commands::Add => {
                    // generate resources from db-compose.toml file
                    println!("Hello, world!");
                }
                Commands::ApplySnapshot => {
                    // applies a snapshot version in a given environment
                    println!("Hello, world!");
                }
                Commands::InstallOrUpdatePortal => {
                    install_or_update_portal(config_path, &iam_config, releaser_path, package_path)
                        .await
                }
                Commands::Deploy => todo!(),
                Commands::Start => {
                    start::main(token.clone(), response, &metadata_config).await;
                },
            }

            // println!("Token is valid: {:?}", response)
        }
        Err(error) => {
            println!("Token validation failed: {:?}", error);
            std::process::exit(1);
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
