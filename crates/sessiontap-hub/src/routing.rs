use crate::{config::Subscription, ingest::AcceptedUpdate};
use sessiontap_core::protocol::{HUB_SCHEMA_VERSION, SourceEnvelope};
use std::process::Stdio;
use tokio::{io::AsyncWriteExt, process::Command};

fn enum_string<T: serde::Serialize>(value: T) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_default()
}

pub fn matches(subscription: &Subscription, update: &AcceptedUpdate) -> bool {
    let criteria = &subscription.match_criteria;
    if !criteria.sources.is_empty() && !criteria.sources.contains(&update.source_id) {
        return false;
    }
    if !criteria.providers.is_empty() && !criteria.providers.contains(&update.view.provider) {
        return false;
    }
    if !criteria.statuses.is_empty()
        && !criteria.statuses.contains(&enum_string(update.view.status))
    {
        return false;
    }
    if !criteria.reasons.is_empty() {
        let reason = update
            .view
            .reason
            .as_ref()
            .map(|reason| enum_string(reason.kind));
        if reason.is_none_or(|reason| !criteria.reasons.contains(&reason)) {
            return false;
        }
    }
    if !criteria.repositories.is_empty() {
        let root = update
            .view
            .repository
            .as_ref()
            .map(|repository| repository.root.as_str());
        if root.is_none_or(|root| {
            !criteria
                .repositories
                .iter()
                .any(|candidate| candidate == root)
        }) {
            return false;
        }
    }
    subscription.changes.is_empty()
        || subscription.changes.iter().any(|field| {
            update
                .changed
                .iter()
                .any(|changed| enum_string(*changed) == *field)
        })
}

pub fn canonical_envelope(update: &AcceptedUpdate) -> SourceEnvelope {
    SourceEnvelope::Update {
        schema_version: HUB_SCHEMA_VERSION,
        source_id: update.source_id.clone(),
        delivery_id: update.delivery_id.clone(),
        revision: update.source_revision,
        changed: update.changed.clone(),
        view: Box::new(update.view.clone()),
    }
}

pub fn environment(update: &AcceptedUpdate) -> Vec<(String, String)> {
    let view = &update.view;
    let mut vars = vec![
        ("SESSIONTAP_SOURCE".into(), update.source_id.clone()),
        ("SESSIONTAP_DELIVERY_ID".into(), update.delivery_id.clone()),
        (
            "SESSIONTAP_HUB_REVISION".into(),
            update.hub_revision.to_string(),
        ),
        (
            "SESSIONTAP_SOURCE_REVISION".into(),
            update.source_revision.to_string(),
        ),
        ("SESSIONTAP_PROVIDER".into(), view.provider.clone()),
        ("SESSIONTAP_STATUS".into(), enum_string(view.status)),
        (
            "SESSIONTAP_INVOCATION_ID".into(),
            view.invocation_id.to_string(),
        ),
        (
            "SESSIONTAP_CHANGED".into(),
            update
                .changed
                .iter()
                .map(|field| enum_string(*field))
                .collect::<Vec<_>>()
                .join(","),
        ),
    ];
    if let Some(session) = &view.session {
        vars.push(("SESSIONTAP_SESSION_ID".into(), session.id.clone()));
        if let Some(name) = &session.name {
            vars.push(("SESSIONTAP_SESSION_NAME".into(), name.clone()));
        }
    }
    if let Some(repository) = &view.repository {
        vars.push(("SESSIONTAP_REPOSITORY_ROOT".into(), repository.root.clone()));
        if let Some(branch) = &repository.branch {
            vars.push(("SESSIONTAP_REPOSITORY_BRANCH".into(), branch.clone()));
        }
    }
    if let Some(reason) = &view.reason {
        vars.push(("SESSIONTAP_REASON_KIND".into(), enum_string(reason.kind)));
        vars.push(("SESSIONTAP_REASON_SUMMARY".into(), reason.summary.clone()));
    }
    vars
}

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
        stdin
            .write_all(&serde_json::to_vec(&canonical_envelope(update))?)
            .await?;
    }
    child.wait().await
}

pub fn dispatch(subscriptions: Vec<Subscription>, update: AcceptedUpdate) {
    tokio::spawn(async move {
        for subscription in subscriptions
            .iter()
            .filter(|subscription| matches(subscription, &update))
        {
            for command in &subscription.commands {
                match execute(command, &update).await {
                    Ok(status) if status.success() => {}
                    Ok(status) => eprintln!(
                        "sessiontap-hub: subscription command {:?} exited with {status} for delivery '{}'",
                        command, update.delivery_id
                    ),
                    Err(error) => eprintln!(
                        "sessiontap-hub: subscription command {:?} failed for delivery '{}': {error}",
                        command, update.delivery_id
                    ),
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{MatchCriteria, Subscription};
    use chrono::Utc;
    use sessiontap_core::domain::{
        InvocationId, PublicAgentView, PublicField, PublicReasonKind, PublicStatus,
        PublicStatusReason,
    };
    use std::collections::BTreeSet;

    fn update() -> AcceptedUpdate {
        AcceptedUpdate {
            hub_revision: 2,
            source_id: "sandbox".into(),
            delivery_id: "d1".into(),
            source_revision: 7,
            view: PublicAgentView {
                invocation_id: InvocationId::new(),
                provider: "codex".into(),
                status: PublicStatus::Blocked,
                reason: Some(PublicStatusReason {
                    kind: PublicReasonKind::Input,
                    summary: "Choose".into(),
                }),
                cwd: "/tmp".into(),
                created_at: Utc::now(),
                updated_at: Utc::now(),
                session: None,
                metadata: None,
                usage: None,
                repository: None,
            },
            changed: BTreeSet::from([PublicField::Reason]),
            first_seen: false,
        }
    }
    #[test]
    fn public_criteria_and_changed_fields_match() {
        let subscription = Subscription {
            name: None,
            match_criteria: MatchCriteria {
                sources: vec!["sandbox".into()],
                providers: vec!["codex".into()],
                statuses: vec!["blocked".into()],
                reasons: vec!["input".into()],
                repositories: vec![],
            },
            changes: vec!["reason".into()],
            commands: vec![vec!["true".into()]],
        };
        assert!(matches(&subscription, &update()));
    }

    #[test]
    fn completed_subscription_matches_response_but_not_lifecycle_only_stop() {
        let subscription = Subscription {
            name: Some("completed".into()),
            match_criteria: MatchCriteria {
                statuses: vec!["stopped".into()],
                reasons: vec!["completed".into()],
                ..Default::default()
            },
            changes: vec!["status".into(), "reason".into()],
            commands: vec![vec!["true".into()]],
        };
        let mut completed = update();
        completed.view.status = PublicStatus::Stopped;
        completed.view.reason = Some(PublicStatusReason {
            kind: PublicReasonKind::Completed,
            summary: "All tests pass".into(),
        });
        completed.changed = BTreeSet::from([PublicField::Status, PublicField::Reason]);
        assert!(matches(&subscription, &completed));
        let vars = environment(&completed);
        assert!(vars.contains(&("SESSIONTAP_REASON_KIND".into(), "completed".into())));
        assert!(vars.contains(&("SESSIONTAP_REASON_SUMMARY".into(), "All tests pass".into())));

        completed.view.reason = None;
        assert!(!matches(&subscription, &completed));
    }
}
