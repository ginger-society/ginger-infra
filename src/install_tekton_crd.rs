use std::collections::HashMap;
use std::io::Write;
use std::process::{Command, Stdio};

use ginger_infra::remote_task::RemoteTask;
use kube::CustomResourceExt;

use crate::run_dry_run::{find_envrc_bounded, parse_envrc};

const DEFAULT_CONTROLLER_IMAGE: &str = "gingersociety/remote-task-controller:latest";
const DEFAULT_RUNNER_IMAGE: &str = "gingersociety/external-executor-runner:latest";
const CONTROLLER_NAMESPACE: &str = "tekton-pipelines";
const CONTROLLER_NAME: &str = "remote-task-controller";
const AUTH_SECRET_NAME: &str = "ginger-society-auth";

pub fn run_install_tekton_crd(
    image: Option<&str>,
    sidekick_url: Option<&str>,
    runner_image: Option<&str>,
) -> anyhow::Result<()> {
    let sidekick_url = sidekick_url
        .ok_or_else(|| anyhow::anyhow!("--sidekick-url is required"))?;

    println!("── Generating RemoteTask CRD + controller manifests ─");

    let crd_yaml = render_crd()?;
    println!("  ✓ RemoteTask CRD schema generated");

    let rbac_yaml = render_rbac();
    println!("  ✓ RBAC manifests generated");

    let controller_image = image.unwrap_or(DEFAULT_CONTROLLER_IMAGE);
    let deployment_yaml = render_deployment(controller_image, sidekick_url);
    println!("  ✓ Controller Deployment generated (image: {controller_image})");

    let runner_img = runner_image.unwrap_or(DEFAULT_RUNNER_IMAGE);
    let tekton_task_yaml = render_tekton_task(sidekick_url, runner_img);
    println!("  ✓ Tekton Task 'remote-task' generated (runner: {runner_img})");

    let combined = format!(
        "{}\n---\n{}\n---\n{}\n---\n{}",
        crd_yaml.trim_end(),
        rbac_yaml.trim_end(),
        deployment_yaml.trim_end(),
        tekton_task_yaml.trim_end(),
    );

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
            println!("\n✓ RemoteTask CRD, RBAC, controller, and Tekton Task installed.");
            println!("  Verify CRD:        kubectl get crd remotetasks.gingersociety.org");
            println!("  Verify controller: kubectl -n {CONTROLLER_NAMESPACE} get deployment {CONTROLLER_NAME}");
            println!("  Verify Task:       kubectl -n {CONTROLLER_NAMESPACE} get task remote-task");
            println!();
            println!("  Create the auth secret (once per namespace):");
            println!("    kubectl create secret generic {AUTH_SECRET_NAME} \\");
            println!("      --from-literal=auth.json='{{\"API_TOKEN\":\"<your-token>\"}}' \\");
            println!("      -n <your-namespace>");
            Ok(())
        }
        false => anyhow::bail!("kubectl apply failed — see output above"),
    }
}

fn render_crd() -> anyhow::Result<String> {
    let crd = RemoteTask::crd();
    serde_yaml::to_string(&crd)
        .map_err(|e| anyhow::anyhow!("Failed to serialize CRD to YAML: {}", e))
}

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
  - apiGroups: ["tekton.dev"]
    resources: ["taskruns"]
    verbs: ["get", "list", "watch", "create", "delete"]
  - apiGroups: ["tekton.dev"]
    resources: ["customruns"]
    verbs: ["get", "list", "watch", "update", "patch"]
  - apiGroups: ["tekton.dev"]
    resources: ["customruns/status"]
    verbs: ["get", "update", "patch"]
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

fn render_deployment(image: &str, sidekick_url: &str) -> String {
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
      securityContext:
        runAsNonRoot: true
        seccompProfile:
          type: RuntimeDefault
      containers:
        - name: controller
          image: {image}
          imagePullPolicy: Always
          securityContext:
            allowPrivilegeEscalation: false
            capabilities:
              drop: ["ALL"]
          env:
            - name: SIDEKICK_URL
              value: "{sidekick_url}"
            # AUTH_SECRET_NAME is the name of the Secret containing auth.json.
            # Defaults to "ginger-society-auth" if unset.
            # - name: AUTH_SECRET_NAME
            #   value: "ginger-society-auth"
            # RUNNER_IMAGE controls which image the TaskRun steps run.
            # Defaults to gingersociety/external-executor-runner:latest if unset.
            # - name: RUNNER_IMAGE
            #   value: "gingersociety/external-executor-runner:latest"
"#,
        name = CONTROLLER_NAME,
        ns = CONTROLLER_NAMESPACE,
        image = image,
        sidekick_url = sidekick_url,
    )
}

/// A reusable Tekton Task for pipeline authors who prefer taskRef over the CRD.
/// Installed alongside the controller so developers have both options.
fn render_tekton_task(sidekick_url: &str, runner_image: &str) -> String {
    format!(
        r#"apiVersion: tekton.dev/v1
kind: Task
metadata:
  name: remote-task
  namespace: {ns}
spec:
  params:
    - name: capability
      type: string
      default: "unix"
    - name: script
      type: string
      description: "Shell script to run on the remote device"
    - name: cleanup
      type: string
      default: ""
      description: "Optional cleanup script to run after the main script"
  steps:
    - name: run
      image: {runner_image}
      env:
        - name: REMOTE_CAPABILITY
          value: $(params.capability)
        - name: REMOTE_SCRIPT
          value: $(params.script)
        - name: REMOTE_CLEANUP
          value: $(params.cleanup)
        - name: EXTERNAL_EXECUTOR_URL
          value: "{sidekick_url}"
      volumeMounts:
        - name: ginger-auth
          mountPath: /var/run/ginger-society
          readOnly: true
  volumes:
    - name: ginger-auth
      secret:
        secretName: {auth_secret}
"#,
        ns = CONTROLLER_NAMESPACE,
        runner_image = runner_image,
        sidekick_url = sidekick_url,
        auth_secret = AUTH_SECRET_NAME,
    )
}

fn kubectl_apply_stdin(
    content: &str,
    env_vars: &HashMap<String, String>,
) -> anyhow::Result<bool> {
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