use sessiontap_core::protocol::{HUB_SCHEMA_VERSION, HubEnvelope};
use std::process::Stdio;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::config::Subscription;
use crate::ingest::AcceptedUpdate;

fn kind_str(kind: &sessiontap_core::domain::EventKind) -> String {
    serde_json::to_value(kind)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_default()
}

fn status_str(status: &sessiontap_core::domain::PublicStatus) -> String {
    serde_json::to_value(status)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_default()
}

fn lifecycle_str(lifecycle: &sessiontap_core::domain::Lifecycle) -> String {
    serde_json::to_value(lifecycle)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_default()
}

/// Different fields combine with logical AND; values inside one field combine
/// with logical OR. An empty field matches everything.
pub fn matches(subscription: &Subscription, update: &AcceptedUpdate) -> bool {
    let criteria = &subscription.match_criteria;
    if !criteria.sources.is_empty() && !criteria.sources.contains(&update.source_id) {
        return false;
    }
    if !criteria.providers.is_empty() && !criteria.providers.contains(&update.snapshot.provider) {
        return false;
    }
    if !criteria.events.is_empty() && !criteria.events.contains(&kind_str(&update.event.kind)) {
        return false;
    }
    if !criteria.statuses.is_empty()
        && !criteria
            .statuses
            .contains(&status_str(&update.snapshot.status))
    {
        return false;
    }
    if !criteria.lifecycles.is_empty()
        && !criteria
            .lifecycles
            .contains(&lifecycle_str(&update.snapshot.lifecycle))
    {
        return false;
    }
    if !criteria.repositories.is_empty() {
        let root = update.snapshot.repository.as_ref().map(|r| r.root.as_str());
        match root {
            Some(root) if criteria.repositories.iter().any(|r| r == root) => {}
            _ => return false,
        }
    }
    if subscription.changes.is_empty() {
        return true;
    }
    subscription
        .changes
        .iter()
        .any(|field| update.changed.iter().any(|changed| changed == field))
}

/// Canonical envelope supplied to commands on stdin: the accepted update in
/// the same versioned shape delivered by the source.
pub fn canonical_envelope(update: &AcceptedUpdate) -> HubEnvelope {
    HubEnvelope::Update {
        schema_version: HUB_SCHEMA_VERSION,
        source_id: update.source_id.clone(),
        event_id: update.event_id.clone(),
        revision: update.snapshot.revision,
        event: update.event.clone(),
        snapshot: Box::new(update.snapshot.clone()),
        attention: update.attention.clone(),
    }
}

/// Documented scalar conveniences exported alongside the stdin envelope.
pub fn environment(update: &AcceptedUpdate) -> Vec<(String, String)> {
    let snapshot = &update.snapshot;
    let mut vars = vec![
        ("SESSIONTAP_SOURCE".into(), update.source_id.clone()),
        ("SESSIONTAP_EVENT_ID".into(), update.event_id.clone()),
        (
            "SESSIONTAP_HUB_REVISION".into(),
            update.hub_revision.to_string(),
        ),
        ("SESSIONTAP_EVENT".into(), kind_str(&update.event.kind)),
        ("SESSIONTAP_PROVIDER".into(), snapshot.provider.clone()),
        ("SESSIONTAP_STATUS".into(), status_str(&snapshot.status)),
        (
            "SESSIONTAP_LIFECYCLE".into(),
            lifecycle_str(&snapshot.lifecycle),
        ),
        (
            "SESSIONTAP_INVOCATION_ID".into(),
            snapshot.invocation_id.to_string(),
        ),
        ("SESSIONTAP_CHANGED".into(), update.changed.join(",")),
    ];
    if let Some(session) = &snapshot.provider_session {
        vars.push(("SESSIONTAP_SESSION_ID".into(), session.id.clone()));
        if let Some(name) = &session.name {
            vars.push(("SESSIONTAP_SESSION_NAME".into(), name.clone()));
        }
    }
    if let Some(repository) = &snapshot.repository {
        vars.push(("SESSIONTAP_REPOSITORY_ROOT".into(), repository.root.clone()));
        if let Some(branch) = &repository.branch {
            vars.push(("SESSIONTAP_REPOSITORY_BRANCH".into(), branch.clone()));
        }
    }
    if let Some(attention) = &update.attention {
        vars.push((
            "SESSIONTAP_ATTENTION_KIND".into(),
            kind_str(&attention.kind),
        ));
        vars.push((
            "SESSIONTAP_ATTENTION_SUMMARY".into(),
            attention.context.summary.clone(),
        ));
        vars.push((
            "SESSIONTAP_ATTENTION_SOURCE".into(),
            serde_json::to_value(attention.context.source)
                .ok()
                .and_then(|v| v.as_str().map(str::to_owned))
                .unwrap_or_default(),
        ));
    }
    vars
}

/// Executes one command argument array directly without shell evaluation.
/// The canonical envelope is provided on stdin. Failures are reported to the
/// caller and never reject an already accepted ingestion.
pub async fn execute(
    command: &[String],
    update: &AcceptedUpdate,
) -> std::io::Result<std::process::ExitStatus> {
    let (program, args) = command
        .split_first()
        .expect("configuration validation rejects empty commands");
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .envs(environment(update))
        .spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        let payload = serde_json::to_vec(&canonical_envelope(update))?;
        stdin.write_all(&payload).await?;
        drop(stdin);
    }
    child.wait().await
}

/// Best-effort dispatch for one accepted update. Runs in its own task so a
/// slow command cannot block ingestion.
pub fn dispatch(subscriptions: Vec<Subscription>, update: AcceptedUpdate) {
    tokio::spawn(async move {
        for subscription in subscriptions.iter().filter(|s| matches(s, &update)) {
            for command in &subscription.commands {
                match execute(command, &update).await {
                    Ok(status) if status.success() => {}
                    Ok(status) => {
                        eprintln!(
                            "sessiontap-hub: subscription command {:?} exited with {status} for event '{}'",
                            command, update.event_id
                        );
                    }
                    Err(error) => {
                        eprintln!(
                            "sessiontap-hub: subscription command {:?} failed for event '{}': {error}",
                            command, update.event_id
                        );
                    }
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::MatchCriteria;
    use chrono::Utc;
    use sessiontap_core::{
        domain::{
            Activity, AttentionContext, AttentionSource, Capabilities, EventKind, InvocationId,
            Lifecycle, ProcessMetadata, PublicStatus, Repository,
        },
        protocol::HubEventMetadata,
    };

    fn update(changed: Vec<&str>) -> AcceptedUpdate {
        let now = Utc::now();
        AcceptedUpdate {
            hub_revision: 1,
            source_id: "sandbox".into(),
            event_id: "event-1".into(),
            event: HubEventMetadata {
                kind: EventKind::WaitingInput,
                observed_at: now,
                received_at: now,
                failure: None,
            },
            snapshot: sessiontap_core::domain::InvocationSnapshot {
                schema_version: 1,
                revision: 3,
                invocation_id: InvocationId::new(),
                provider: "codex".into(),
                executable: "codex".into(),
                args: vec![],
                cwd: "/work".into(),
                process: ProcessMetadata::default(),
                created_at: now,
                updated_at: now,
                lifecycle: Lifecycle::Alive,
                activity: Activity::WaitingInput,
                status: PublicStatus::Blocked,
                provider_session: Some(sessiontap_core::domain::ProviderSession {
                    id: "session-9".into(),
                    name: Some("demo".into()),
                }),
                usage: None,
                repository: Some(Repository {
                    root: "/work/project".into(),
                    branch: Some("main".into()),
                    head: None,
                    dirty: None,
                }),
                multiplexer: None,
                capabilities: Capabilities::default(),
                turn_generation: 0,
                completed_generation: None,
            },
            attention: Some(sessiontap_core::domain::ActiveAttention {
                kind: EventKind::WaitingInput,
                context: AttentionContext {
                    summary: "Choose an option".into(),
                    source: AttentionSource::Question,
                },
            }),
            changed: changed.into_iter().map(str::to_owned).collect(),
            first_seen: false,
        }
    }

    fn subscription(criteria: MatchCriteria, changes: Vec<&str>) -> Subscription {
        Subscription {
            name: None,
            match_criteria: criteria,
            changes: changes.into_iter().map(str::to_owned).collect(),
            commands: vec![vec!["true".into()]],
        }
    }

    #[test]
    fn multi_field_rule_requires_every_criterion() {
        let sub = subscription(
            MatchCriteria {
                sources: vec!["sandbox".into()],
                providers: vec!["codex".into()],
                events: vec!["waiting_input".into()],
                statuses: vec!["blocked".into()],
                ..Default::default()
            },
            vec![],
        );
        assert!(matches(&sub, &update(vec!["attention"])));
        let mismatched_source = subscription(
            MatchCriteria {
                sources: vec!["host".into()],
                providers: vec!["codex".into()],
                events: vec!["waiting_input".into()],
                statuses: vec!["blocked".into()],
                ..Default::default()
            },
            vec![],
        );
        assert!(!matches(&mismatched_source, &update(vec![])));
    }

    #[test]
    fn values_within_one_field_are_ored() {
        let sub = subscription(
            MatchCriteria {
                providers: vec!["claude".into(), "codex".into()],
                statuses: vec!["running".into(), "blocked".into()],
                ..Default::default()
            },
            vec![],
        );
        assert!(matches(&sub, &update(vec![])));
    }

    #[test]
    fn repository_filter_requires_matching_root() {
        let sub = subscription(
            MatchCriteria {
                repositories: vec!["/work/project".into()],
                ..Default::default()
            },
            vec![],
        );
        assert!(matches(&sub, &update(vec![])));
        let other = subscription(
            MatchCriteria {
                repositories: vec!["/elsewhere".into()],
                ..Default::default()
            },
            vec![],
        );
        assert!(!matches(&other, &update(vec![])));
    }

    #[test]
    fn changes_filter_watches_material_field_changes() {
        let sub = subscription(MatchCriteria::default(), vec!["status", "attention"]);
        assert!(matches(&sub, &update(vec!["attention"])));
        assert!(!matches(&sub, &update(vec!["usage"])));
        assert!(!matches(&sub, &update(vec![])));
        let no_filter = subscription(MatchCriteria::default(), vec![]);
        assert!(matches(&no_filter, &update(vec!["usage"])));
    }

    #[test]
    fn canonical_envelope_round_trips_with_explicit_attention() {
        let update = update(vec!["attention"]);
        let envelope = canonical_envelope(&update);
        let value = serde_json::to_value(&envelope).unwrap();
        assert_eq!(value["type"], "update");
        assert_eq!(value["source_id"], "sandbox");
        assert_eq!(value["attention"]["kind"], "waiting_input");
        let cleared = AcceptedUpdate {
            attention: None,
            ..update
        };
        let value = serde_json::to_value(canonical_envelope(&cleared)).unwrap();
        assert!(value["attention"].is_null());
    }

    #[test]
    fn environment_exports_documented_scalars() {
        let vars = environment(&update(vec!["status", "attention"]));
        let lookup = |key: &str| vars.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone());
        assert_eq!(lookup("SESSIONTAP_SOURCE").as_deref(), Some("sandbox"));
        assert_eq!(lookup("SESSIONTAP_PROVIDER").as_deref(), Some("codex"));
        assert_eq!(lookup("SESSIONTAP_EVENT").as_deref(), Some("waiting_input"));
        assert_eq!(lookup("SESSIONTAP_STATUS").as_deref(), Some("blocked"));
        assert_eq!(lookup("SESSIONTAP_LIFECYCLE").as_deref(), Some("alive"));
        assert_eq!(
            lookup("SESSIONTAP_SESSION_ID").as_deref(),
            Some("session-9")
        );
        assert_eq!(
            lookup("SESSIONTAP_ATTENTION_SUMMARY").as_deref(),
            Some("Choose an option")
        );
        assert_eq!(
            lookup("SESSIONTAP_REPOSITORY_ROOT").as_deref(),
            Some("/work/project")
        );
        assert_eq!(
            lookup("SESSIONTAP_CHANGED").as_deref(),
            Some("status,attention")
        );
    }

    #[tokio::test]
    async fn command_receives_envelope_on_stdin_and_env() {
        let temp = tempfile::tempdir().unwrap();
        let out = temp.path().join("captured.json");
        let env_out = temp.path().join("env.txt");
        let update = update(vec!["attention"]);
        let status = execute(
            &[
                "/bin/sh".into(),
                "-c".into(),
                format!(
                    "cat > {} && printf '%s' \"$SESSIONTAP_SOURCE/$SESSIONTAP_EVENT\" > {}",
                    out.display(),
                    env_out.display()
                ),
            ],
            &update,
        )
        .await
        .unwrap();
        assert!(status.success());
        let captured: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&out).unwrap()).unwrap();
        assert_eq!(captured["type"], "update");
        assert_eq!(captured["event_id"], "event-1");
        assert_eq!(
            std::fs::read_to_string(&env_out).unwrap(),
            "sandbox/waiting_input"
        );
    }

    #[tokio::test]
    async fn shell_metacharacters_are_literal_arguments() {
        let temp = tempfile::tempdir().unwrap();
        let update = update(vec![]);
        // A literal metacharacter argument must never reach a shell: `true`
        // ignores argv, and the side-effect file must not be created.
        let status = execute(
            &[
                "true".into(),
                format!("a b; touch {}", temp.path().join("pwned").display()),
            ],
            &update,
        )
        .await
        .unwrap();
        assert!(status.success());
        assert!(!temp.path().join("pwned").exists());
    }
}
