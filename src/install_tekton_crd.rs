// src/install_tekton_crd.rs
//
// `ginger-infra install-tekton-crd` — one-shot bootstrap command. Generates:
//   1. the RemoteTask CustomResourceDefinition (from the Rust types in
//      remote_task.rs, via kube::CustomResourceExt — the schema is derived
//      from the struct, so the CRD on the cluster can never drift from
//      what the controller actually understands)
//   2. RBAC for the controller (ServiceAccount, ClusterRole, ClusterRoleBinding)
//   3. the controller Deployment itself
//
// All three are applied via `kubectl apply -f -`, the same pattern rollout.rs
// uses for normal manifests — nothing is written to disk, and .envrc/KUBECONFIG
// resolution follows the same find_envrc_bounded convention as everywhere else
// in this CLI, scoped to the current directory.
//
// This is a one-shot "make this cluster capable of RemoteTask" operation —
// the same category as install_helm_charts and install_or_update_portal —
// not something that needs to run on every reconcile.

use std::collections::HashMap;
use std::io::Write;
use std::process::{Command, Stdio};

use ginger_infra::remote_task::RemoteTask;
use kube::CustomResourceExt;

use crate::run_dry_run::{find_envrc_bounded, parse_envrc};

/// Controller image to deploy. Override via --image if the user builds
/// and pushes their own tag; this default assumes the project publishes
/// to the same registry pattern as other ginger-society images.
const DEFAULT_CONTROLLER_IMAGE: &str = "gingersociety/remote-task-controller:latest";
const CONTROLLER_NAMESPACE: &str = "tekton-pipelines";
const CONTROLLER_NAME: &str = "remote-task-controller";

pub fn run_install_tekton_crd(image: Option<&str>, sidekick_url: Option<&str>) -> anyhow::Result<()> {
    println!("── Generating RemoteTask CRD + controller manifests ─");

    let crd_yaml = render_crd()?;
    println!("  ✓ RemoteTask CRD schema generated from remote_task.rs types");

    let rbac_yaml = render_rbac();
    println!("  ✓ RBAC manifests generated");

    let controller_image = image.unwrap_or(DEFAULT_CONTROLLER_IMAGE);
    let deployment_yaml = render_deployment(controller_image, sidekick_url);
    println!("  ✓ Controller Deployment generated (image: {})", controller_image);

    // Concatenate as a multi-document YAML stream — kubectl apply -f -
    // handles `---`-separated documents in a single stdin payload.
    let combined = format!(
        "{}\n---\n{}\n---\n{}",
        crd_yaml.trim_end(),
        rbac_yaml.trim_end(),
        deployment_yaml.trim_end()
    );

    // resolve .envrc for KUBECONFIG, bounded by cwd — same convention as
    // every other command that shells out to kubectl in this CLI
    let cwd = std::env::current_dir()?;
    let env_vars = match find_envrc_bounded(&cwd, &cwd) {
        Some(envrc_path) => {
            let content = std::fs::read_to_string(&envrc_path)
                .map_err(|e| anyhow::anyhow!("Cannot read .envrc: {}", e))?;
            println!("  ✓ .envrc loaded from {}", envrc_path.display());
            parse_envrc(&content)
        }
        None => {
            println!("  ⚠ No .envrc found — using inherited environment");
            HashMap::new()
        }
    };

    println!("\n── Applying to cluster ──────────────────────────────");
    match kubectl_apply_stdin(&combined, &env_vars)? {
        true => {
            println!("\n✓ RemoteTask CRD, RBAC, and controller installed.");
            println!("  Verify with: kubectl get crd remotetasks.gingersociety.org");
            println!("  Verify with: kubectl -n {} get deployment {}", CONTROLLER_NAMESPACE, CONTROLLER_NAME);
            Ok(())
        }
        false => anyhow::bail!("kubectl apply failed — see output above"),
    }
}

/// Generate the CRD YAML directly from the Rust spec/status types, so the
/// schema the apiserver validates against can never drift from what the
/// controller's serde structs actually deserialize.
fn render_crd() -> anyhow::Result<String> {
    let crd = RemoteTask::crd();
    serde_yaml::to_string(&crd)
        .map_err(|e| anyhow::anyhow!("Failed to serialize generated CRD to YAML: {}", e))
}

/// RBAC the controller needs: get/list/watch on RemoteTask (+ update status),
/// get on Secret (env resolution), and basic event recording. Deliberately
/// scoped to exactly what install_tekton_crd.rs's reconcile loop touches —
/// expand only when the controller's actual code needs more.
fn render_rbac() -> String {
    format!(
        r#"apiVersion: v1
kind: ServiceAccount
metadata:
  name: {name}
  namespace: {ns}
---
apiVersion: rbac.authorization.k8s.io/v1
kind: ClusterRole
metadata:
  name: {name}
rules:
  - apiGroups: ["gingersociety.org"]
    resources: ["remotetasks"]
    verbs: ["get", "list", "watch", "update", "patch"]
  - apiGroups: ["gingersociety.org"]
    resources: ["remotetasks/status"]
    verbs: ["get", "update", "patch"]
  - apiGroups: [""]
    resources: ["secrets"]
    verbs: ["get"]
  - apiGroups: [""]
    resources: ["events"]
    verbs: ["create", "patch"]
---
apiVersion: rbac.authorization.k8s.io/v1
kind: ClusterRoleBinding
metadata:
  name: {name}
roleRef:
  apiGroup: rbac.authorization.k8s.io
  kind: ClusterRole
  name: {name}
subjects:
  - kind: ServiceAccount
    name: {name}
    namespace: {ns}
"#,
        name = CONTROLLER_NAME,
        ns = CONTROLLER_NAMESPACE,
    )
}

fn render_deployment(image: &str, sidekick_url: Option<&str>) -> String {
    let sidekick_env = match sidekick_url {
        Some(url) => format!(
            r#"
            - name: SIDEKICK_URL
              value: "{}""#,
            url
        ),
        None => String::new(),
    };

    format!(
        r#"apiVersion: apps/v1
kind: Deployment
metadata:
  name: {name}
  namespace: {ns}
spec:
  replicas: 1
  selector:
    matchLabels:
      app: {name}
  template:
    metadata:
      labels:
        app: {name}
    spec:
      serviceAccountName: {name}
      containers:
        - name: controller
          image: {image}
          env:{sidekick_env}
"#,
        name = CONTROLLER_NAME,
        ns = CONTROLLER_NAMESPACE,
        image = image,
        sidekick_env = sidekick_env,
    )
}

/// Pipe `content` into `kubectl apply -f -`. Mirrors rollout.rs's
/// kubectl_apply_stdin exactly — nothing written to disk, env vars
/// (KUBECONFIG etc.) injected from the resolved .envrc.
fn kubectl_apply_stdin(content: &str, env_vars: &HashMap<String, String>) -> anyhow::Result<bool> {
    let mut cmd = Command::new("kubectl");
    cmd.args(["apply", "-f", "-"]);
    cmd.stdin(Stdio::piped());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    for (k, v) in env_vars {
        cmd.env(k, v);
    }

    let mut child = cmd
        .spawn()
        .map_err(|e| anyhow::anyhow!("Failed to spawn kubectl: {}", e))?;

    if let Some(stdin) = child.stdin.take() {
        let mut stdin = stdin;
        stdin
            .write_all(content.as_bytes())
            .map_err(|e| anyhow::anyhow!("Failed to write to kubectl stdin: {}", e))?;
    }

    let output = child
        .wait_with_output()
        .map_err(|e| anyhow::anyhow!("kubectl did not exit cleanly: {}", e))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    if output.status.success() {
        for line in stdout.lines() {
            println!("    {}", line);
        }
        Ok(true)
    } else {
        eprintln!("  ✗ kubectl apply failed (exit {:?})", output.status.code());
        for line in stderr.lines() {
            eprintln!("    {}", line);
        }
        Ok(false)
    }
}