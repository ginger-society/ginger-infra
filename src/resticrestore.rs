use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub const PHASE_PENDING: &str = "Pending";
pub const PHASE_WAITING_FOR_SCALE_DOWN: &str = "WaitingForScaleDown";
pub const PHASE_RUNNING: &str = "Running";
pub const PHASE_SUCCEEDED: &str = "Succeeded";
pub const PHASE_FAILED: &str = "Failed";

#[derive(CustomResource, Deserialize, Serialize, Clone, Debug, JsonSchema)]
#[kube(
    group = "gingersociety.org",
    version = "v1alpha1",
    kind = "ResticRestore",
    plural = "resticrestores",
    shortname = "rr",
    namespaced,
    status = "ResticRestoreStatus"
)]
#[serde(rename_all = "camelCase")]
pub struct ResticRestoreSpec {
    /// Name of the PVC to restore into. Must exist and be empty (or set
    /// clean_existing_data) before this CR is created.
    pub pvc_name: String,
    /// Restic snapshot ID to restore. Defaults to "latest" if omitted.
    #[serde(default)]
    pub snapshot_id: Option<String>,
    /// If true, the restore Job deletes existing contents of the mount
    /// before restoring. If false, restic restores on top of what's there.
    #[serde(default)]
    pub clean_existing_data: bool,
}

#[derive(Deserialize, Serialize, Clone, Debug, Default, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ResticRestoreStatus {
    #[serde(default)]
    pub phase: String,
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub completion_time: Option<String>,
}

impl ResticRestore {
    pub fn snapshot_ref(&self) -> String {
        self.spec.snapshot_id.clone().unwrap_or_else(|| "latest".to_string())
    }

    pub fn is_terminal(&self) -> bool {
        self.status
            .as_ref()
            .map(|s| s.phase == PHASE_SUCCEEDED || s.phase == PHASE_FAILED)
            .unwrap_or(false)
    }
}