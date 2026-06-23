use k8s_openapi::api::core::v1::{Event, EventSource, ObjectReference};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{ObjectMeta, Time};
use kube::{api::PostParams, Api, Client};

const REPORTING_COMPONENT: &str = "tekton-controller";

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

    // Hash message content + nanoseconds to guarantee a unique name
    // even for multiple lines arriving within the same second
    let hash = {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let mut h = DefaultHasher::new();
        message.hash(&mut h);
        now.0.subsec_nanosecond().hash(&mut h);
        // also mix in the target name so concurrent tasks don't collide
        target.name.hash(&mut h);
        h.finish()
    };

    let event_name = format!(
        "{}.{:x}",
        target.name.as_deref().unwrap_or("unknown"),
        hash,
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