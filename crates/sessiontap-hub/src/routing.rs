use crate::{
    config::{HubConfig, Subscription},
    ingest::AcceptedUpdate,
};
use sessiontap_core::protocol::{HUB_SCHEMA_VERSION, SourceEnvelope};
use std::{process::Stdio, sync::Arc, time::Duration};
use tokio::{io::AsyncWriteExt, process::Command, sync::Semaphore, task::JoinHandle};

pub fn matches(subscription: &Subscription, update: &AcceptedUpdate) -> bool {
    let criteria = &subscription.match_criteria;
    if !criteria.sources.is_empty() && !criteria.sources.contains(&update.source_id) {
        return false;
    }
    if !criteria.providers.is_empty() && !criteria.providers.contains(&update.view.provider) {
        return false;
    }
    if !criteria.statuses.is_empty() && !criteria.statuses.contains(&update.view.status) {
        return false;
    }
    if !criteria.reasons.is_empty()
        && update
            .view
            .reason
            .as_ref()
            .is_none_or(|reason| !criteria.reasons.contains(&reason.kind))
    {
        return false;
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
        || subscription
            .changes
            .iter()
            .any(|field| update.changed.contains(field))
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
        ("SESSIONTAP_STATUS".into(), view.status.as_str().into()),
        (
            "SESSIONTAP_INVOCATION_ID".into(),
            view.invocation_id.to_string(),
        ),
        (
            "SESSIONTAP_CHANGED".into(),
            update
                .changed
                .iter()
                .map(|field| field.as_str())
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
        vars.push(("SESSIONTAP_REASON_KIND".into(), reason.kind.as_str().into()));
        vars.push(("SESSIONTAP_REASON_SUMMARY".into(), reason.summary.clone()));
    }
    vars
}

/// Bounds subscription command execution: at most `permits` commands run at
/// once across all deliveries, and each is killed after `timeout`.
#[derive(Debug, Clone)]
pub struct CommandLimits {
    permits: Arc<Semaphore>,
    timeout: Duration,
}

impl CommandLimits {
    #[must_use]
    pub fn new(max_concurrent: usize, timeout: Duration) -> Self {
        Self {
            permits: Arc::new(Semaphore::new(max_concurrent.max(1))),
            timeout,
        }
    }

    #[must_use]
    pub fn from_config(config: &HubConfig) -> Self {
        Self::new(
            config.max_concurrent_commands,
            Duration::from_secs(config.command_timeout_secs),
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandOutcome {
    Exited(std::process::ExitStatus),
    /// The command exceeded its timeout and was killed.
    TimedOut,
}

/// Runs one command with the canonical envelope on stdin. Writing stdin and
/// waiting both count toward `timeout`; on expiry the child is killed.
pub async fn execute(
    command: &[String],
    update: &AcceptedUpdate,
    timeout: Duration,
) -> std::io::Result<CommandOutcome> {
    let (program, args) = command
        .split_first()
        .expect("configuration validation rejects empty commands");
    let payload = serde_json::to_vec(&canonical_envelope(update))?;
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .envs(environment(update))
        .kill_on_drop(true)
        .spawn()?;
    let stdin = child.stdin.take();
    let run = async {
        if let Some(mut stdin) = stdin {
            stdin.write_all(&payload).await?;
        }
        child.wait().await
    };
    match tokio::time::timeout(timeout, run).await {
        Ok(status) => Ok(CommandOutcome::Exited(status?)),
        Err(_) => {
            let _ = child.kill().await;
            Ok(CommandOutcome::TimedOut)
        }
    }
}

/// Runs every matching subscription's commands for one accepted update.
/// Commands for this update run in configuration order; each waits for a
/// concurrency permit, so excess commands queue rather than being dropped.
pub fn dispatch(
    subscriptions: Arc<Vec<Subscription>>,
    update: AcceptedUpdate,
    limits: CommandLimits,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        for subscription in subscriptions
            .iter()
            .filter(|subscription| matches(subscription, &update))
        {
            for command in &subscription.commands {
                let Ok(_permit) = limits.permits.acquire().await else {
                    return;
                };
                match execute(command, &update, limits.timeout).await {
                    Ok(CommandOutcome::Exited(status)) if status.success() => {}
                    Ok(CommandOutcome::Exited(status)) => eprintln!(
                        "sessiontap-hub: subscription command {:?} exited with {status} for delivery '{}'",
                        command, update.delivery_id
                    ),
                    Ok(CommandOutcome::TimedOut) => eprintln!(
                        "sessiontap-hub: subscription command {:?} timed out after {}s for delivery '{}'; killed",
                        command,
                        limits.timeout.as_secs_f64(),
                        update.delivery_id
                    ),
                    Err(error) => eprintln!(
                        "sessiontap-hub: subscription command {:?} failed for delivery '{}': {error}",
                        command, update.delivery_id
                    ),
                }
            }
        }
    })
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
                statuses: vec![PublicStatus::Blocked],
                reasons: vec![PublicReasonKind::Input],
                repositories: vec![],
            },
            changes: vec![PublicField::Reason],
            commands: vec![vec!["true".into()]],
        };
        assert!(matches(&subscription, &update()));
    }

    #[test]
    fn completed_subscription_matches_response_but_not_lifecycle_only_stop() {
        let subscription = Subscription {
            name: Some("completed".into()),
            match_criteria: MatchCriteria {
                statuses: vec![PublicStatus::Stopped],
                reasons: vec![PublicReasonKind::Completed],
                ..Default::default()
            },
            changes: vec![PublicField::Status, PublicField::Reason],
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

    fn shell_subscription(commands: &[&str]) -> Arc<Vec<Subscription>> {
        Arc::new(vec![Subscription {
            name: None,
            match_criteria: MatchCriteria::default(),
            changes: vec![],
            commands: commands
                .iter()
                .map(|script| vec!["sh".into(), "-c".into(), (*script).into()])
                .collect(),
        }])
    }

    #[tokio::test]
    async fn hanging_command_is_killed_at_timeout() {
        let started = std::time::Instant::now();
        let outcome = execute(
            &["sleep".into(), "60".into()],
            &update(),
            Duration::from_millis(300),
        )
        .await
        .unwrap();
        assert_eq!(outcome, CommandOutcome::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[tokio::test]
    async fn dispatch_continues_after_timeout() {
        let temp = tempfile::tempdir().unwrap();
        let log = temp.path().join("log");
        let after = format!("echo after >> '{}'", log.display());
        dispatch(
            shell_subscription(&["sleep 60", &after]),
            update(),
            CommandLimits::new(1, Duration::from_millis(200)),
        )
        .await
        .unwrap();
        assert_eq!(std::fs::read_to_string(&log).unwrap(), "after\n");
    }

    #[tokio::test]
    async fn burst_runs_at_most_limit_commands_and_keeps_order() {
        let temp = tempfile::tempdir().unwrap();
        let log = temp.path().join("log");
        let first = format!(
            "echo \"start $SESSIONTAP_DELIVERY_ID\" >> '{0}'; sleep 0.2; echo \"end $SESSIONTAP_DELIVERY_ID\" >> '{0}'",
            log.display()
        );
        let second = format!(
            "echo \"second $SESSIONTAP_DELIVERY_ID\" >> '{}'",
            log.display()
        );
        let subscriptions = shell_subscription(&[&first, &second]);
        let limits = CommandLimits::new(2, Duration::from_secs(10));
        let handles: Vec<_> = (0..5)
            .map(|index| {
                let mut update = update();
                update.delivery_id = format!("d{index}");
                dispatch(subscriptions.clone(), update, limits.clone())
            })
            .collect();
        for handle in handles {
            handle.await.unwrap();
        }
        let log = std::fs::read_to_string(&log).unwrap();
        let (mut running, mut peak) = (0_i32, 0_i32);
        for line in log.lines() {
            match line.split_once(' ').unwrap().0 {
                "start" => running += 1,
                "end" => running -= 1,
                _ => {}
            }
            peak = peak.max(running);
        }
        assert!(peak <= 2, "peak concurrency {peak}:\n{log}");
        assert!(peak >= 1);
        for index in 0..5 {
            let lines: Vec<&str> = log.lines().collect();
            let end = lines.iter().position(|l| *l == format!("end d{index}"));
            let second = lines.iter().position(|l| *l == format!("second d{index}"));
            assert!(
                end.is_some() && second.is_some(),
                "delivery d{index} dropped:\n{log}"
            );
            assert!(end < second, "order broken for d{index}:\n{log}");
        }
    }
}
