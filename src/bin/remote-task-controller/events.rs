//! Small helper to publish Kubernetes `Event` objects against an arbitrary
//! `ObjectReference`. Used so that:
//!   - the RemoteTask controller can mirror each `log` SSE line it receives
//!     from sidekick onto the originating RemoteTask (and, by extension, the
//!     owning CustomRun) as a real k8s Event, and
//!   - the CustomRun controller can announce lifecycle events (created,
//!     etc.) on the CustomRun object itself.
//!
//! Events are how `tkn pr logs` / the Tekton dashboard can show *something*
//! for a CustomRun-backed step even though no pod ever runs for it.
//!
//! NOTE: this uses the core/v1 `Event` type directly via `Api<Event>` rather
//! than `kube::runtime::events::Recorder`, to avoid pulling in the
//! EventsV1 (events.k8s.io) feature surface — adjust if your kube feature
//! flags already include the `events` helper and you'd rather use that.

use k8s_openapi::api::core::v1::{Event, EventSource, ObjectReference};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{ObjectMeta, Time};
use kube::{api::PostParams, Api, Client};

const REPORTING_COMPONENT: &str = "tekton-controller";

/// Emits a single Event against `target` (Normal or Warning `type_`).
/// Best-effort: callers should generally log-and-continue on error rather
/// than fail the whole reconcile over a missed event.
pub async fn emit_event(
    client: &Client,
    ns: &str,
    target: &ObjectReference,
    type_: &str,
    reason: &str,
    message: &str,
) -> Result<(), kube::Error> {
    let events: Api<Event> = Api::namespaced(client.clone(), ns);

    let now = Time(k8s_openapi::jiff::Timestamp::now());

    // Event names must be unique; target name + a coarse timestamp is good
    // enough here since we don't need exact dedup/aggregation semantics.
    let event_name = format!(
        "{}.{}",
        target.name.as_deref().unwrap_or("unknown"),
        now.0.as_second()
    );

    let event = Event {
        metadata: ObjectMeta {
            name: Some(event_name),
            namespace: Some(ns.to_string()),
            ..Default::default()
        },
        involved_object: target.clone(),
        type_: Some(type_.to_string()),
        reason: Some(reason.to_string()),
        message: Some(message.to_string()),
        source: Some(EventSource {
            component: Some(REPORTING_COMPONENT.to_string()),
            ..Default::default()
        }),
        first_timestamp: Some(now.clone()),
        last_timestamp: Some(now),
        count: Some(1),
        ..Default::default()
    };

    events.create(&PostParams::default(), &event).await?;
    Ok(())
}