use std::{path::Path, process::exit};

use colored::Colorize;
use ginger_shared_rs::{read_service_config_file, utils::get_package_json_info};
use IAMService::{
    apis::{
        configuration::Configuration as IAMConfiguration,
        default_api::{identity_create_or_update_app, IdentityCreateOrUpdateAppParams},
    },
    models::CreateOrUpdateAppRequest,
};

pub async fn install_or_update_portal(
    service_config_path: &Path,
    iam_config: &IAMConfiguration,
    releaser_path: &Path,
    package_path: &Path,
) {
    let services_config = match read_service_config_file(service_config_path) {
        Ok(c) => c,
        Err(e) => {
            println!("{:?}", e);
            println!(
                "There is no service configuration found or the existing one is invalid. Please use {} to add one. Exiting",
                "ginger-connector init".blue()
            );
            exit(1);
        }
    };
    println!("{:?}", services_config.portal_config);
    println!("{:?}", services_config.urls);

    // Properly handle the result of `get_package_json_info()`
    let (pkg_name, version, description, organization, internal_dependencies) =
        if let Some(info) = get_package_json_info() {
            info
        } else {
            println!(
                "{}",
                "Failed to retrieve package.json information. Exiting.".red()
            );
            exit(1);
        };

    match identity_create_or_update_app(
        &iam_config,
        IdentityCreateOrUpdateAppParams {
            create_or_update_app_request: CreateOrUpdateAppRequest {
                client_id: services_config.portal_config.clone().unwrap().id,
                name: Some(Some(
                    services_config.portal_config.clone().unwrap().friendly_name,
                )),
                logo_url: Some(Some(
                    services_config.portal_config.clone().unwrap().logo_url,
                )),
                disabled: Some(Some(
                    services_config.portal_config.clone().unwrap().disabled,
                )),
                app_url_dev: Some(
                    services_config
                        .urls
                        .as_ref()
                        .and_then(|urls| urls.get("dev").cloned()),
                ),
                app_url_stage: Some(
                    services_config
                        .urls
                        .as_ref()
                        .and_then(|urls| urls.get("stage").cloned()),
                ),
                app_url_prod: Some(
                    services_config
                        .urls
                        .as_ref()
                        .and_then(|urls| urls.get("prod").cloned()),
                ),
                group_id: Some(
                    services_config
                        .portal_config
                        .clone()
                        .unwrap()
                        .access_group_id,
                ),
                tnc_link: Some(services_config.portal_config.clone().unwrap().tnc_url),
                allow_registration: Some(Some(
                    services_config
                        .portal_config
                        .clone()
                        .unwrap()
                        .allow_registration,
                )),
                description: Some(Some(description)),
                auth_redirection_path: Some(
                    services_config
                        .portal_config
                        .clone()
                        .unwrap()
                        .auth_redirection_path,
                ),
                web_interface: Some(Some(
                    services_config
                        .portal_config
                        .clone()
                        .unwrap()
                        .has_web_interface,
                )),
            },
        },
    )
    .await
    {
        Ok(resp) => {
            println!("{:?}", resp)
        }
        Err(err) => {
            println!("{:?}", err)
        }
    }
}
