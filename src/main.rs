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

mod portalInstaller;

#[derive(Subcommand, Debug)]
enum Commands {
    /// Initialize a infra project
    Init,
    /// adds env
    AddEnv,
    /// adds redis
    AddCache,
    /// adds rabbitMQ
    AddMessageQueue,
    /// only the exporter
    AddRDBMS,
    /// Apply
    Apply,
    /// build all the infra base images, basically triggers the CI pipeline in the infra repo
    Build,
    /// build cli in the current working dir , this requires the GH_TOKEN env to be present in the current shell session. Supposed to be a dev machine but can also be triggered as part of pipeline which is building a component the infra dependes on
    BuildCli,
    /// upload cli , this requires AWS credentials to be present in the system , supposed to be run on a dev machine.
    UploadCli,
    /// installs a portal , creates an entry in the application table in dev portal
    InstallOrUpdatePortal,
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

#[tokio::main]
async fn check_session_gurad(
    cli: Args,
    config_path: &Path,
    iam_config: &IAMConfiguration,
    metadata_config: &MetadataConfiguration,
    package_path: &Path,
    releaser_path: &Path,
) {
    match identity_validate_api_token(&iam_config).await {
        Ok(response) => {
            match cli.command {
                Commands::Init => {
                    println!("Hello, world!");
                }
                Commands::AddEnv => {
                    //  create a folder in environments folder and adds ingress , ssl and other common resources
                    println!("Hello, world!");
                }
                Commands::AddCache => {
                    // adds reddis cache and deployment service
                    println!("Hello, world!");
                }
                Commands::AddMessageQueue => {
                    // adds rabbitmq deployment and service
                    println!("Hello, world!");
                }
                Commands::AddRDBMS => {
                    // only the exporter deployment and service, or in future a multi read and write replica using helm could be possible
                    println!("Hello, world!");
                }
                Commands::Apply => {
                    // applies a snapshot version in a given environment
                    println!("Hello, world!");
                }
                Commands::Build => todo!(),
                Commands::BuildCli => todo!(),
                Commands::UploadCli => todo!(),
                Commands::InstallOrUpdatePortal => {
                    install_or_update_portal(config_path, &iam_config, releaser_path, package_path)
                        .await
                }
            }

            // println!("Token is valid: {:?}", response)
        }
        Err(error) => {
            println!("Token validation failed: {:?}", error);
            std::process::exit(1);
        }
    }
}

fn main() {
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
    );
}
