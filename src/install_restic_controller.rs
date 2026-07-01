use anyhow::{bail, Context, Result};
use k8s_openapi::api::apps::v1::{Deployment, DeploymentSpec};
use k8s_openapi::api::core::v1::{Container, EnvVar, PodSpec, PodTemplateSpec, Secret, ServiceAccount};
use k8s_openapi::api::rbac::v1::{
    ClusterRole, ClusterRoleBinding, PolicyRule, Role, RoleBinding, RoleRef, Subject,
};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{LabelSelector, ObjectMeta};
use kube::api::{Api, Patch, PatchParams};
use kube::Client;
use std::collections::BTreeMap;

const FIELD_MANAGER: &str = "ginger-infra";
const CONTROLLER_NAME: &str = "restic-snapshot-controller";
const REQUIRED_SECRET_KEYS: [&str; 3] = ["AWS_ACCESS_KEY_ID", "AWS_SECRET_ACCESS_KEY", "RESTIC_PASSWORD"];

pub async fn run_install_restic_controller(
    image: Option<&str>,
    s3_base_path: &str,
    schedule: Option<&str>,
    credentials_secret_name: Option<&str>,
    namespace: Option<&str>,
) -> Result<()> {
    let client = Client::try_default().await.context("connecting to cluster")?;
    let ns = namespace.unwrap_or("default");
    let image = image.unwrap_or("gingersociety/restic-snapshot-controller:latest");
    let schedule = schedule.unwrap_or("0 0 * * * *");
    let secret_name = credentials_secret_name.unwrap_or("s3-credentials");

    check_credentials_secret(&client, ns, secret_name).await?;

    apply_service_account(&client, ns).await?;
    apply_cluster_role(&client).await?;
    apply_cluster_role_binding(&client, ns).await?;
    apply_secret_role(&client, ns, secret_name).await?;
    apply_secret_role_binding(&client, ns).await?;
    apply_deployment(&client, ns, image, s3_base_path, schedule, secret_name).await?;

    println!(
        "[install-restic-controller] installed {CONTROLLER_NAME} in namespace '{ns}' (image={image}, schedule='{schedule}')"
    );
    println!(
        "[install-restic-controller] to enable backup on a PVC, annotate it:\n\
         \n\
         \tkubectl annotate pvc <pvc-name> -n <namespace> snapshot.gingersociety.org/enabled=\"true\"\n"
    );
    Ok(())
}

/// Verifies the credentials secret exists in the controller's namespace and
/// has all required keys. Fails fast with a copy-pasteable fix instead of
/// installing a controller that will error at cron-time.
async fn check_credentials_secret(client: &Client, ns: &str, secret_name: &str) -> Result<()> {
    let secrets: Api<Secret> = Api::namespaced(client.clone(), ns);

    let secret = match secrets.get(secret_name).await {
        Ok(s) => s,
        Err(kube::Error::Api(e)) if e.code == 404 => {
            print_secret_missing_snippet(ns, secret_name);
            bail!("secret '{ns}/{secret_name}' not found");
        }
        Err(e) => return Err(e).context(format!("checking secret {ns}/{secret_name}")),
    };

    let data = secret.data.unwrap_or_default();
    let missing: Vec<&str> = REQUIRED_SECRET_KEYS
        .iter()
        .filter(|k| !data.contains_key(**k))
        .copied()
        .collect();

    if !missing.is_empty() {
        eprintln!(
            "[install-restic-controller] secret '{ns}/{secret_name}' exists but is missing keys: {}",
            missing.join(", ")
        );
        print_secret_missing_snippet(ns, secret_name);
        bail!("secret '{ns}/{secret_name}' is missing required keys: {}", missing.join(", "));
    }

    println!("[install-restic-controller] found secret '{ns}/{secret_name}' with all required keys");
    Ok(())
}

fn print_secret_missing_snippet(ns: &str, secret_name: &str) {
    eprintln!(
        "\n\
         [install-restic-controller] required secret not ready. Create it with:\n\
         \n\
         \tkubectl create secret generic {secret_name} -n {ns} \\\n\
         \t  --from-literal=AWS_ACCESS_KEY_ID=<your-access-key> \\\n\
         \t  --from-literal=AWS_SECRET_ACCESS_KEY=<your-secret-key> \\\n\
         \t  --from-literal=RESTIC_PASSWORD=<a-strong-password-you-will-not-lose>\n\
         \n\
         Then re-run this command.\n"
    );
}

async fn apply_service_account(client: &Client, ns: &str) -> Result<()> {
    let api: Api<ServiceAccount> = Api::namespaced(client.clone(), ns);
    let sa = ServiceAccount {
        metadata: ObjectMeta {
            name: Some(CONTROLLER_NAME.into()),
            namespace: Some(ns.into()),
            ..Default::default()
        },
        ..Default::default()
    };
    api.patch(CONTROLLER_NAME, &PatchParams::apply(FIELD_MANAGER), &Patch::Apply(&sa))
        .await?;
    Ok(())
}

async fn apply_cluster_role(client: &Client) -> Result<()> {
    let api: Api<ClusterRole> = Api::all(client.clone());
    let role = ClusterRole {
        metadata: ObjectMeta {
            name: Some(CONTROLLER_NAME.into()),
            ..Default::default()
        },
        rules: Some(vec![
            PolicyRule {
                api_groups: Some(vec!["".into()]),
                resources: Some(vec!["persistentvolumeclaims".into()]),
                verbs: vec!["get".into(), "list".into(), "watch".into()],
                ..Default::default()
            },
            PolicyRule {
                api_groups: Some(vec!["".into()]),
                resources: Some(vec!["pods".into(), "pods/log".into()]),
                verbs: vec!["get".into(), "list".into(), "watch".into()],
                ..Default::default()
            },
            PolicyRule {
                api_groups: Some(vec!["batch".into()]),
                resources: Some(vec!["jobs".into()]),
                verbs: vec![
                    "get".into(),
                    "list".into(),
                    "watch".into(),
                    "create".into(),
                    "delete".into(),
                ],
                ..Default::default()
            },
        ]),
        ..Default::default()
    };
    api.patch(CONTROLLER_NAME, &PatchParams::apply(FIELD_MANAGER), &Patch::Apply(&role))
        .await?;
    Ok(())
}

async fn apply_cluster_role_binding(client: &Client, ns: &str) -> Result<()> {
    let api: Api<ClusterRoleBinding> = Api::all(client.clone());
    let binding = ClusterRoleBinding {
        metadata: ObjectMeta {
            name: Some(CONTROLLER_NAME.into()),
            ..Default::default()
        },
        role_ref: RoleRef {
            api_group: "rbac.authorization.k8s.io".into(),
            kind: "ClusterRole".into(),
            name: CONTROLLER_NAME.into(),
        },
        subjects: Some(vec![Subject {
            kind: "ServiceAccount".into(),
            name: CONTROLLER_NAME.into(),
            namespace: Some(ns.into()),
            ..Default::default()
        }]),
    };
    api.patch(CONTROLLER_NAME, &PatchParams::apply(FIELD_MANAGER), &Patch::Apply(&binding))
        .await?;
    Ok(())
}

/// Namespaced Role, scoped to exactly the one secret the controller needs —
/// deliberately not folded into the ClusterRole above, since secret access
/// should stay tightly bound to a specific resourceName, not "any secret
/// in any namespace".
async fn apply_secret_role(client: &Client, ns: &str, secret_name: &str) -> Result<()> {
    let api: Api<Role> = Api::namespaced(client.clone(), ns);
    let role = Role {
        metadata: ObjectMeta {
            name: Some(format!("{CONTROLLER_NAME}-secrets")),
            namespace: Some(ns.into()),
            ..Default::default()
        },
        rules: Some(vec![PolicyRule {
            api_groups: Some(vec!["".into()]),
            resources: Some(vec!["secrets".into()]),
            resource_names: Some(vec![secret_name.into()]),
            verbs: vec!["get".into()],
            ..Default::default()
        }]),
    };
    api.patch(
        &format!("{CONTROLLER_NAME}-secrets"),
        &PatchParams::apply(FIELD_MANAGER),
        &Patch::Apply(&role),
    )
    .await?;
    Ok(())
}

async fn apply_secret_role_binding(client: &Client, ns: &str) -> Result<()> {
    let api: Api<RoleBinding> = Api::namespaced(client.clone(), ns);
    let binding = RoleBinding {
        metadata: ObjectMeta {
            name: Some(format!("{CONTROLLER_NAME}-secrets")),
            namespace: Some(ns.into()),
            ..Default::default()
        },
        role_ref: RoleRef {
            api_group: "rbac.authorization.k8s.io".into(),
            kind: "Role".into(),
            name: format!("{CONTROLLER_NAME}-secrets"),
        },
        subjects: Some(vec![Subject {
            kind: "ServiceAccount".into(),
            name: CONTROLLER_NAME.into(),
            namespace: Some(ns.into()),
            ..Default::default()
        }]),
    };
    api.patch(
        &format!("{CONTROLLER_NAME}-secrets"),
        &PatchParams::apply(FIELD_MANAGER),
        &Patch::Apply(&binding),
    )
    .await?;
    Ok(())
}

async fn apply_deployment(
    client: &Client,
    ns: &str,
    image: &str,
    s3_base_path: &str,
    schedule: &str,
    secret_name: &str,
) -> Result<()> {
    let api: Api<Deployment> = Api::namespaced(client.clone(), ns);

    let mut labels = BTreeMap::new();
    labels.insert("app".to_string(), CONTROLLER_NAME.to_string());

    let env = vec![
        EnvVar { name: "S3_BASE_PATH".into(), value: Some(s3_base_path.into()), ..Default::default() },
        EnvVar { name: "CRON_SCHEDULE".into(), value: Some(schedule.into()), ..Default::default() },
        EnvVar { name: "CREDENTIALS_SECRET_NAME".into(), value: Some(secret_name.into()), ..Default::default() },
        EnvVar { name: "CONTROLLER_NAMESPACE".into(), value: Some(ns.into()), ..Default::default() },
    ];

    let deployment = Deployment {
        metadata: ObjectMeta {
            name: Some(CONTROLLER_NAME.into()),
            namespace: Some(ns.into()),
            labels: Some(labels.clone()),
            ..Default::default()
        },
        spec: Some(DeploymentSpec {
            replicas: Some(1),
            selector: LabelSelector {
                match_labels: Some(labels.clone()),
                ..Default::default()
            },
            template: PodTemplateSpec {
                metadata: Some(ObjectMeta {
                    labels: Some(labels),
                    ..Default::default()
                }),
                spec: Some(PodSpec {
                    service_account_name: Some(CONTROLLER_NAME.into()),
                    containers: vec![Container {
                        name: CONTROLLER_NAME.into(),
                        image: Some(image.into()),
                        env: Some(env),
                        ..Default::default()
                    }],
                    ..Default::default()
                }),
            },
            ..Default::default()
        }),
        ..Default::default()
    };

    api.patch(CONTROLLER_NAME, &PatchParams::apply(FIELD_MANAGER), &Patch::Apply(&deployment))
        .await?;
    Ok(())
}