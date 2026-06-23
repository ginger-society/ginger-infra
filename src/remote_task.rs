// src/remote_task.rs
//
// CRD types for `RemoteTask` — a Kubernetes-native way to declare a job
// that runs on a device reachable via the sidekick service (the same
// /run-job SSE endpoint that `ginger-infra rpc` talks to), rather than
// inside a container in the cluster.
//
// Scope, deliberately narrow for the first version:
//   - env resolution supports only `value` and `secretKeyRef` — no
//     configMapKeyRef, no Tekton `$(params.x)` / `$(context.x)` expression
//     resolution yet. Both are real future work, not implemented here.
//   - no cancellation handling (deleting/cancelling the owning PipelineRun
//     does not currently signal the device to stop the job)
//   - no resume-on-restart (if the controller restarts mid-job, the SSE
//     connection is lost and the RemoteTask is left in Running forever —
//     see the controller's reconcile loop for the TODO marking this)
//   - no workspace/cache handling — the script is responsible for any
//     git clone / artifact fetch / cache reuse it needs. RemoteTask only
//     describes "what to run", never "what data to bring along".
//
// These boundaries are intentional, not oversights — see the design
// conversation that produced this file for the reasoning. Expand scope
// here only after the narrow version is proven against a real cluster.

use k8s_openapi::apimachinery::pkg::apis::meta::v1::Time;
use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

// ── env var source ─────────────────────────────────────────────────────────

/// Reference to a key within a Secret, mirroring the shape of a normal
/// Kubernetes Pod's `env[].valueFrom.secretKeyRef` (but only the two
/// fields we actually need).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
pub struct SecretKeyRef {
    /// Name of the Secret in the same namespace as the RemoteTask
    pub name: String,
    /// Key within the Secret's data
    pub key: String,
}

/// A single env var entry. Exactly one of `value` or `value_from` should
/// be set — this mirrors Tekton/Pod env syntax closely enough that authors
/// familiar with either will recognize it immediately.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
pub struct RemoteTaskEnvVar {
    pub name: String,
    /// A literal value. NOTE: Tekton expressions like `$(params.x)` are
    /// NOT resolved here in v1 — if you write one, it is forwarded to the
    /// device verbatim as that literal string. Use plain values only until
    /// expression resolution is implemented.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    /// Pull the value from a Secret in the same namespace at reconcile time.
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "valueFrom")]
    pub value_from: Option<RemoteTaskEnvVarSource>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
pub struct RemoteTaskEnvVarSource {
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "secretKeyRef")]
    pub secret_key_ref: Option<SecretKeyRef>,
}

// ── spec ──────────────────────────────────────────────────────────────────

#[derive(CustomResource, Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
#[kube(
    group = "gingersociety.org",
    version = "v1alpha1",
    kind = "RemoteTask",
    plural = "remotetasks",
    singular = "remotetask",
    shortname = "rtask",
    namespaced,
    status = "RemoteTaskStatus",
    printcolumn = r#"{"name":"Capability", "type":"string", "jsonPath":".spec.capability"}"#,
    printcolumn = r#"{"name":"Phase", "type":"string", "jsonPath":".status.phase"}"#,
    printcolumn = r#"{"name":"ExitCode", "type":"integer", "jsonPath":".status.exitCode"}"#,
    printcolumn = r#"{"name":"Age", "type":"date", "jsonPath":".metadata.creationTimestamp"}"#
)]
#[serde(rename_all = "camelCase")]
pub struct RemoteTaskSpec {
    /// Which device capability to target, e.g. "unix", "osxarm64".
    /// Matches the `capability` field the presence service matches devices on.
    pub capability: String,

    /// Base URL of the sidekick service's /run-job endpoint, e.g.
    /// "http://tekton-sidekick.infra.svc.cluster.local:8099/run-job".
    /// Kept explicit per-RemoteTask (rather than only a controller-wide
    /// default) so a single controller can serve multiple sidekick
    /// deployments/namespaces if needed later.
    pub sidekick_url: String,

    /// Env vars resolved before dispatch. Only `value` and
    /// `valueFrom.secretKeyRef` are supported in this version.
    #[serde(default)]
    pub env: Vec<RemoteTaskEnvVar>,

    /// The script to run on the remote device.
    pub script: String,

    /// Optional cleanup script — forwarded as-is to the device, which runs
    /// it unconditionally after the main script (success or failure).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cleanup: Option<String>,
}

// ── status ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Default)]
#[serde(rename_all = "camelCase")]
pub enum RemoteTaskPhase {
    #[default]
    Pending,
    Running,
    Succeeded,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Default)]
#[serde(rename_all = "camelCase")]
pub struct RemoteTaskCondition {
    #[serde(rename = "type")]
    pub type_: String,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Default)]
#[serde(rename_all = "camelCase")]
pub struct RemoteTaskStatus {
    #[serde(default)]
    pub phase: RemoteTaskPhase,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// WAMP channel of the device the job was dispatched to, once known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_channel: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_time: Option<Time>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion_time: Option<Time>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default)]
    pub conditions: Vec<RemoteTaskCondition>,
}