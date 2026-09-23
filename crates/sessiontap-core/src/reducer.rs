//! Pure invocation state transitions. Every decision about how an event or a
//! local observation changes an invocation lives here; storage only loads the
//! prior row, assigns the revision, and persists what this module returns.

use crate::domain::{
    Activity, ActivityConfirmation, CHILD_AGENT_ID_MAX_CHARS, CHILD_AGENT_TYPE_MAX_CHARS,
    CHILD_AGENTS_MAX, COLLECTOR_INSTANCE_ID_MAX_CHARS, ChildActivity, ChildAgentRef,
    ChildAgentState, CurrentStatusReason, CurrentToolActivity, EventEvidence, EventKind,
    EvidenceChannel, EvidenceTrust, InvocationSnapshot, Lifecycle, NormalizedEvent,
    ProviderSession, PublicAgentView, PublicChildAgentReason, PublicField, PublicReasonKind,
    SOURCE_ORDER_CURSOR_MAX, STATUS_REASON_MAX_BYTES, STATUS_REASON_MAX_CHARS, SourceOrderCursor,
    StatusReasonContext, TOOL_CORRELATION_ID_MAX_CHARS, TOOL_DETAIL_MAX_CHARS,
    TOOL_LABEL_MAX_CHARS, ToolActivityPhase, ToolActivityUpdate, changed_public_fields,
    derive_status, project_public,
};
use anyhow::{Result, bail};
use chrono::{DateTime, Duration, Utc};
use std::collections::BTreeSet;

pub const STALE_WORKING_MINUTES: i64 = 30;

/// Committed state a transition starts from.
#[derive(Debug, Clone, Copy)]
pub struct Prior<'a> {
    pub snapshot: &'a InvocationSnapshot,
    pub reason: Option<&'a CurrentStatusReason>,
}

/// What a transition does to the private current status reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReasonEffect {
    Keep,
    Clear,
    Set(CurrentStatusReason),
}

impl ReasonEffect {
    /// The current reason after applying this effect to `prior`.
    #[must_use]
    pub fn resolve<'a>(
        &'a self,
        prior: Option<&'a CurrentStatusReason>,
    ) -> Option<&'a CurrentStatusReason> {
        match self {
            Self::Keep => prior,
            Self::Clear => None,
            Self::Set(reason) => Some(reason),
        }
    }
}

/// The next snapshot and reason effect. `revision` and `updated_at` are not
/// yet assigned; storage applies them through [`finalize`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transition {
    pub snapshot: InvocationSnapshot,
    pub reason: ReasonEffect,
    pub suppressed: bool,
}

/// Rejects events whose evidence, tool activity, or status reason are not
/// bounded normalized data.
pub fn validate_event(
    event: &NormalizedEvent,
    status_reason: Option<&StatusReasonContext>,
) -> Result<()> {
    validate_evidence(&event.evidence)?;
    validate_tool_activity(event)?;
    validate_child_agent(event)?;
    if status_reason.is_some_and(|context| {
        context.summary.is_empty()
            || context.summary.len() > STATUS_REASON_MAX_BYTES
            || context.summary.chars().count() > STATUS_REASON_MAX_CHARS
            || context.summary.chars().any(char::is_control)
    }) {
        bail!("status reason context is not bounded normalized text");
    }
    Ok(())
}

/// Applies one normalized provider event to the prior state.
pub fn apply_event(
    prior: Prior<'_>,
    event: &NormalizedEvent,
    status_reason: Option<&StatusReasonContext>,
) -> Result<Transition> {
    validate_event(event, status_reason)?;
    if let Some(child) = &event.child_agent {
        return Ok(apply_child_event(prior, event, child));
    }
    let mut snapshot = prior.snapshot.clone();
    let mut effective = event_with_channel_authority(event);
    if matches!(snapshot.lifecycle, Lifecycle::Exited | Lifecycle::Lost) {
        effective.tool_activity = None;
        if authoritative_activity(&effective.kind, effective.evidence.channel) {
            effective.kind = EventKind::Enrichment;
        }
    }
    let stale_order = is_stale_source_order(&snapshot.source_ordering, &event.evidence);
    let stale_session = effective.provider_session_id.as_ref().is_some_and(|id| {
        snapshot
            .provider_session
            .as_ref()
            .is_some_and(|current| current.id != *id)
            && effective.kind != EventKind::ProviderSessionStarted
    });
    let stale_turn = effective.turn_id.as_ref().is_some_and(|id| {
        snapshot
            .provider_metadata
            .as_ref()
            .and_then(|m| m.current_turn_id.as_ref())
            .is_some_and(|current| current != id)
            && effective.kind != EventKind::NewTurn
    });
    let terminal_for_turn = snapshot.completed_generation == Some(snapshot.turn_generation);
    if terminal_for_turn {
        effective.tool_activity = None;
    }
    let suppressed_terminal_event = terminal_for_turn
        && matches!(
            effective.kind,
            EventKind::Working
                | EventKind::WaitingInput
                | EventKind::WaitingApproval
                | EventKind::Completed
                | EventKind::Failed
                | EventKind::Interrupted
        );
    let suppressed = stale_order || stale_session || stale_turn || suppressed_terminal_event;
    if !suppressed {
        reduce(&mut snapshot, &effective);
    }
    snapshot.status = derive_status(snapshot.lifecycle, snapshot.activity);
    let kind = if suppressed {
        &EventKind::Enrichment
    } else {
        &effective.kind
    };
    let reason = match kind {
        EventKind::WaitingApproval
        | EventKind::WaitingInput
        | EventKind::Completed
        | EventKind::Failed
        | EventKind::Interrupted => status_reason.map_or(ReasonEffect::Clear, |context| {
            ReasonEffect::Set(CurrentStatusReason {
                kind: effective.kind.clone(),
                context: context.clone(),
            })
        }),
        EventKind::NewTurn
        | EventKind::Working
        | EventKind::Idle
        | EventKind::ProviderSessionStarted => ReasonEffect::Clear,
        EventKind::SessionEnded => {
            clear_unless(prior.reason, &[EventKind::Completed, EventKind::Failed])
        }
        EventKind::ProviderSessionEnded | EventKind::Enrichment => ReasonEffect::Keep,
    };
    Ok(Transition {
        snapshot,
        reason,
        suppressed,
    })
}

/// Applies one child-agent event. Only `children` changes; the root turn,
/// session, metadata, usage, tool activity, and reason are untouched. A child
/// may outlive the root's terminal turn, so terminal-turn suppression does not
/// apply, while stale source-order and older-session guards do.
fn apply_child_event(
    prior: Prior<'_>,
    event: &NormalizedEvent,
    child: &ChildAgentRef,
) -> Transition {
    let mut snapshot = prior.snapshot.clone();
    let effective = event_with_channel_authority(event);
    let stale_order = is_stale_source_order(&snapshot.source_ordering, &event.evidence);
    let stale_session = effective.provider_session_id.as_ref().is_some_and(|id| {
        snapshot
            .provider_session
            .as_ref()
            .is_some_and(|current| current.id != *id)
    });
    let suppressed = stale_order || stale_session;
    if !suppressed {
        if !matches!(snapshot.lifecycle, Lifecycle::Exited | Lifecycle::Lost) {
            reduce_child(&mut snapshot, &effective, child);
        }
        record_source_order(&mut snapshot, &event.evidence);
    }
    Transition {
        snapshot,
        reason: ReasonEffect::Keep,
        suppressed,
    }
}

/// Updates, creates, or evicts the child addressed by `child`. Events that
/// carry no child activity leave the list unchanged.
pub fn reduce_child(
    snapshot: &mut InvocationSnapshot,
    event: &NormalizedEvent,
    child: &ChildAgentRef,
) {
    let (activity, kind) = match event.kind {
        EventKind::NewTurn | EventKind::Working => (ChildActivity::Running, None),
        EventKind::WaitingInput => (ChildActivity::Blocked, Some(PublicReasonKind::Input)),
        EventKind::WaitingApproval => (ChildActivity::Blocked, Some(PublicReasonKind::Approval)),
        EventKind::Completed => (ChildActivity::Stopped, Some(PublicReasonKind::Completed)),
        EventKind::Failed | EventKind::Interrupted => {
            (ChildActivity::Stopped, Some(PublicReasonKind::Failed))
        }
        EventKind::Idle
        | EventKind::ProviderSessionStarted
        | EventKind::ProviderSessionEnded
        | EventKind::SessionEnded
        | EventKind::Enrichment => return,
    };
    let label = event.tool_activity.as_ref().map(|tool| tool.label.clone());
    let now = event.received_at;
    let existing = snapshot
        .children
        .iter()
        .position(|state| state.agent_id == child.agent_id);
    let index = if let Some(index) = existing {
        index
    } else {
        if snapshot.children.len() >= CHILD_AGENTS_MAX {
            let Some(oldest_stopped) = snapshot
                .children
                .iter()
                .enumerate()
                .filter(|(_, state)| state.activity == ChildActivity::Stopped)
                .min_by(|(_, a), (_, b)| {
                    a.started_at
                        .cmp(&b.started_at)
                        .then_with(|| a.agent_id.cmp(&b.agent_id))
                })
                .map(|(index, _)| index)
            else {
                return;
            };
            snapshot.children.remove(oldest_stopped);
        }
        snapshot.children.push(ChildAgentState {
            agent_id: child.agent_id.clone(),
            agent_type: child.agent_type.clone(),
            activity,
            reason: None,
            started_at: now,
            updated_at: now,
        });
        snapshot.children.len() - 1
    };
    let state = &mut snapshot.children[index];
    let summary = if activity == ChildActivity::Stopped {
        label
    } else {
        label.or_else(|| {
            state
                .reason
                .as_ref()
                .and_then(|reason| reason.summary.clone())
        })
    };
    state.agent_type.clone_from(&child.agent_type);
    state.activity = activity;
    state.reason =
        (kind.is_some() || summary.is_some()).then_some(PublicChildAgentReason { kind, summary });
    state.updated_at = now;
}

/// The supervised process disappeared without a lifecycle exit.
#[must_use]
pub fn mark_lost(prior: Prior<'_>) -> Transition {
    let mut snapshot = prior.snapshot.clone();
    snapshot.lifecycle = Lifecycle::Lost;
    snapshot.current_tool_activity = None;
    snapshot.children.clear();
    snapshot.last_evidence = Some(EventEvidence::local(EvidenceChannel::ProcessObservation));
    snapshot.status = derive_status(snapshot.lifecycle, snapshot.activity);
    Transition {
        snapshot,
        reason: clear_unless(prior.reason, &[EventKind::Completed, EventKind::Failed]),
        suppressed: false,
    }
}

/// Downgrades working state that has not been asserted for
/// [`STALE_WORKING_MINUTES`] and drops running children silent for as long;
/// returns `None` when nothing is stale. Child expiry claims no outcome and
/// leaves the root lifecycle, activity, and reason untouched.
#[must_use]
pub fn expire_stale_working(prior: Prior<'_>, now: DateTime<Utc>) -> Option<Transition> {
    let root_stale = is_stale_working(prior.snapshot, now);
    if !root_stale && !has_stale_children(prior.snapshot, now) {
        return None;
    }
    let mut snapshot = prior.snapshot.clone();
    snapshot
        .children
        .retain(|child| !is_stale_child(child, now));
    if root_stale {
        snapshot.activity = Activity::Unknown;
        snapshot.state_started_at = now;
        snapshot.current_tool_activity = None;
        snapshot.status = derive_status(snapshot.lifecycle, snapshot.activity);
    }
    Some(Transition {
        snapshot,
        reason: if root_stale {
            ReasonEffect::Clear
        } else {
            ReasonEffect::Keep
        },
        suppressed: false,
    })
}

/// Whether [`expire_stale_working`] would change `snapshot` at `now`.
#[must_use]
pub fn is_stale(snapshot: &InvocationSnapshot, now: DateTime<Utc>) -> bool {
    is_stale_working(snapshot, now) || has_stale_children(snapshot, now)
}

fn has_stale_children(snapshot: &InvocationSnapshot, now: DateTime<Utc>) -> bool {
    snapshot
        .children
        .iter()
        .any(|child| is_stale_child(child, now))
}

/// Whether `child` is running with no accepted child event for too long.
/// Blocked children wait indefinitely; stopped children clear at the next
/// retention boundary.
fn is_stale_child(child: &ChildAgentState, now: DateTime<Utc>) -> bool {
    child.activity == ChildActivity::Running
        && now.signed_duration_since(child.updated_at) >= Duration::minutes(STALE_WORKING_MINUTES)
}

/// Whether `snapshot` is alive, working, and unasserted for too long.
#[must_use]
pub fn is_stale_working(snapshot: &InvocationSnapshot, now: DateTime<Utc>) -> bool {
    let last_asserted = snapshot
        .last_state_asserted_at
        .unwrap_or(snapshot.state_started_at);
    snapshot.lifecycle == Lifecycle::Alive
        && snapshot.activity == Activity::Working
        && now.signed_duration_since(last_asserted) >= Duration::minutes(STALE_WORKING_MINUTES)
}

/// A locally observed process change such as child binding or exit.
/// `clear_incompatible_reason` drops tool activity and any reason other than
/// a terminal outcome.
#[must_use]
pub fn local_mutation(
    prior: Prior<'_>,
    clear_incompatible_reason: bool,
    f: impl FnOnce(&mut InvocationSnapshot),
) -> Transition {
    let mut snapshot = prior.snapshot.clone();
    f(&mut snapshot);
    snapshot.last_evidence = Some(EventEvidence::local(EvidenceChannel::ProcessObservation));
    if clear_incompatible_reason {
        snapshot.current_tool_activity = None;
    }
    if matches!(snapshot.lifecycle, Lifecycle::Exited | Lifecycle::Lost) {
        snapshot.children.clear();
    }
    snapshot.status = derive_status(snapshot.lifecycle, snapshot.activity);
    let reason = if clear_incompatible_reason {
        clear_unless(
            prior.reason,
            &[
                EventKind::Completed,
                EventKind::Failed,
                EventKind::Interrupted,
            ],
        )
    } else {
        ReasonEffect::Keep
    };
    Transition {
        snapshot,
        reason,
        suppressed: false,
    }
}

/// Projects the committed view. `updated_at` is bumped to `now` only when the
/// projection differs from `prior_view`, so the returned field set is
/// non-empty exactly when the observable view changed.
pub fn finalize(
    prior_view: &PublicAgentView,
    snapshot: &mut InvocationSnapshot,
    reason: Option<&CurrentStatusReason>,
    now: DateTime<Utc>,
) -> (PublicAgentView, BTreeSet<PublicField>) {
    let provisional = project_public(snapshot, reason);
    if !changed_public_fields(Some(prior_view), &provisional).is_empty() {
        snapshot.updated_at = now;
    }
    let view = project_public(snapshot, reason);
    let changed = changed_public_fields(Some(prior_view), &view);
    (view, changed)
}

fn clear_unless(reason: Option<&CurrentStatusReason>, retained: &[EventKind]) -> ReasonEffect {
    if reason.is_some_and(|reason| !retained.contains(&reason.kind)) {
        ReasonEffect::Clear
    } else {
        ReasonEffect::Keep
    }
}

pub fn validate_evidence(evidence: &EventEvidence) -> Result<()> {
    let trusted = matches!(
        (evidence.channel, evidence.trust),
        (
            EvidenceChannel::ManagedHook,
            EvidenceTrust::AuthenticatedInvocation
        ) | (
            EvidenceChannel::SideChannel
                | EvidenceChannel::ProcessObservation
                | EvidenceChannel::ProviderArtifact,
            EvidenceTrust::LocalObservation
        )
    );
    if !trusted {
        bail!("evidence channel and trust basis are inconsistent");
    }
    if evidence
        .collector_instance_id
        .as_ref()
        .is_some_and(|value| {
            value.is_empty()
                || value.chars().count() > COLLECTOR_INSTANCE_ID_MAX_CHARS
                || value.chars().any(char::is_control)
        })
    {
        bail!("collector instance identity is not bounded normalized text");
    }
    Ok(())
}

pub fn validate_child_agent(event: &NormalizedEvent) -> Result<()> {
    let bounded = |value: &str, max: usize| {
        !value.is_empty() && value.chars().count() <= max && !value.chars().any(char::is_control)
    };
    if event.child_agent.as_ref().is_some_and(|child| {
        !bounded(&child.agent_id, CHILD_AGENT_ID_MAX_CHARS)
            || !bounded(&child.agent_type, CHILD_AGENT_TYPE_MAX_CHARS)
    }) {
        bail!("child agent identity is not bounded normalized text");
    }
    Ok(())
}

pub fn validate_tool_activity(event: &NormalizedEvent) -> Result<()> {
    if event.tool_activity.as_ref().is_some_and(|tool| {
        tool.label.is_empty()
            || tool.label.chars().count() > TOOL_LABEL_MAX_CHARS
            || tool.label.chars().any(char::is_control)
            || tool.correlation_id.as_ref().is_some_and(|value| {
                value.is_empty()
                    || value.chars().count() > TOOL_CORRELATION_ID_MAX_CHARS
                    || value.chars().any(char::is_control)
            })
            || tool.detail.as_ref().is_some_and(|value| {
                value.is_empty()
                    || value.chars().count() > TOOL_DETAIL_MAX_CHARS
                    || value.chars().any(char::is_control)
            })
    }) {
        bail!("tool activity is not bounded normalized data");
    }
    Ok(())
}

#[must_use]
pub fn event_with_channel_authority(event: &NormalizedEvent) -> NormalizedEvent {
    let mut effective = event.clone();
    match event.evidence.channel {
        EvidenceChannel::ManagedHook | EvidenceChannel::SideChannel => {}
        EvidenceChannel::ProcessObservation => {
            if effective.kind != EventKind::SessionEnded {
                effective.kind = EventKind::Enrichment;
            }
            effective.provider_session_id = None;
            effective.provider_session_name = None;
            effective.provider_session_start_reason = None;
            effective.provider_metadata = None;
            effective.usage = None;
            effective.turn_id = None;
            effective.tool_activity = None;
        }
        EvidenceChannel::ProviderArtifact => {
            effective.kind = EventKind::Enrichment;
            effective.provider_session_start_reason = None;
            effective.turn_id = None;
            effective.tool_activity = None;
            if let Some(metadata) = effective.provider_metadata.as_mut() {
                metadata.permission_mode = None;
                metadata.current_turn_id = None;
            }
        }
    }
    effective
}

#[must_use]
pub fn is_stale_source_order(previous: &[SourceOrderCursor], current: &EventEvidence) -> bool {
    let Some(sequence) = current.source_sequence else {
        return false;
    };
    previous.iter().any(|cursor| {
        cursor.channel == current.channel
            && cursor.collector_revision == current.collector_revision
            && cursor.collector_instance_id == current.collector_instance_id
            && sequence <= cursor.sequence
    })
}

pub fn record_source_order(snapshot: &mut InvocationSnapshot, evidence: &EventEvidence) {
    let Some(sequence) = evidence.source_sequence else {
        return;
    };
    if let Some(cursor) = snapshot.source_ordering.iter_mut().find(|cursor| {
        cursor.channel == evidence.channel
            && cursor.collector_revision == evidence.collector_revision
            && cursor.collector_instance_id == evidence.collector_instance_id
    }) {
        cursor.sequence = sequence;
        return;
    }
    if snapshot.source_ordering.len() == SOURCE_ORDER_CURSOR_MAX {
        snapshot.source_ordering.remove(0);
    }
    snapshot.source_ordering.push(SourceOrderCursor {
        channel: evidence.channel,
        collector_revision: evidence.collector_revision,
        collector_instance_id: evidence.collector_instance_id.clone(),
        sequence,
    });
}

#[must_use]
pub const fn authoritative_activity(kind: &EventKind, channel: EvidenceChannel) -> bool {
    matches!(
        channel,
        EvidenceChannel::ManagedHook | EvidenceChannel::SideChannel
    ) && matches!(
        kind,
        EventKind::NewTurn
            | EventKind::Working
            | EventKind::Idle
            | EventKind::WaitingInput
            | EventKind::WaitingApproval
            | EventKind::Completed
            | EventKind::Failed
            | EventKind::Interrupted
            | EventKind::ProviderSessionStarted
    )
}

#[must_use]
pub fn matching_tool(
    current: &CurrentToolActivity,
    update: &ToolActivityUpdate,
    allow_label_fallback: bool,
) -> bool {
    match (&current.correlation_id, &update.correlation_id) {
        (Some(current), Some(update)) => current == update,
        (None, None) => current.label == update.label,
        _ => allow_label_fallback && current.label == update.label,
    }
}

pub fn reduce_tool_activity(snapshot: &mut InvocationSnapshot, event: &NormalizedEvent) {
    let session_boundary = event.provider_session_id.as_ref().is_some_and(|id| {
        snapshot
            .provider_session
            .as_ref()
            .is_none_or(|session| session.id != *id)
    });
    if session_boundary
        || matches!(
            event.kind,
            EventKind::NewTurn
                | EventKind::Idle
                | EventKind::Completed
                | EventKind::Failed
                | EventKind::Interrupted
                | EventKind::ProviderSessionStarted
                | EventKind::ProviderSessionEnded
                | EventKind::SessionEnded
        )
    {
        snapshot.current_tool_activity = None;
    }
    let Some(update) = &event.tool_activity else {
        return;
    };
    match update.phase {
        ToolActivityPhase::Start => {
            if let Some(current) = snapshot.current_tool_activity.as_mut()
                && matching_tool(current, update, false)
            {
                current.last_observed_at = event.received_at;
                if update.detail.is_some() {
                    current.detail.clone_from(&update.detail);
                }
            } else {
                snapshot.current_tool_activity = Some(CurrentToolActivity {
                    label: update.label.clone(),
                    correlation_id: update.correlation_id.clone(),
                    detail: update.detail.clone(),
                    started_at: event.received_at,
                    last_observed_at: event.received_at,
                });
            }
        }
        ToolActivityPhase::Progress | ToolActivityPhase::Attention => {
            if let Some(current) = snapshot.current_tool_activity.as_mut()
                && matching_tool(current, update, true)
            {
                current.last_observed_at = event.received_at;
                if update.detail.is_some() {
                    current.detail.clone_from(&update.detail);
                }
            }
        }
        ToolActivityPhase::Finish | ToolActivityPhase::Failure => {
            if snapshot
                .current_tool_activity
                .as_ref()
                .is_some_and(|current| matching_tool(current, update, false))
            {
                snapshot.current_tool_activity = None;
            }
        }
    }
}

pub fn reduce(snapshot: &mut InvocationSnapshot, event: &NormalizedEvent) {
    let prior_activity = snapshot.activity;
    reduce_tool_activity(snapshot, event);
    match event.kind {
        EventKind::NewTurn => {
            snapshot.turn_generation += 1;
            snapshot.completed_generation = None;
            snapshot.activity = Activity::Working;
            snapshot.children.clear();
        }
        EventKind::Working if snapshot.completed_generation != Some(snapshot.turn_generation) => {
            snapshot.activity = Activity::Working
        }
        EventKind::Idle => snapshot.activity = Activity::Idle,
        EventKind::WaitingInput
            if snapshot.completed_generation != Some(snapshot.turn_generation) =>
        {
            snapshot.activity = Activity::WaitingInput;
        }
        EventKind::WaitingApproval
            if snapshot.completed_generation != Some(snapshot.turn_generation) =>
        {
            snapshot.activity = Activity::WaitingApproval;
        }
        EventKind::Completed | EventKind::Failed | EventKind::Interrupted => {
            snapshot.activity = Activity::Stopped;
            snapshot.completed_generation = Some(snapshot.turn_generation);
        }
        EventKind::ProviderSessionStarted => {
            snapshot.activity = Activity::Idle;
        }
        EventKind::ProviderSessionEnded => {}
        EventKind::SessionEnded => {
            snapshot.lifecycle = Lifecycle::Exited;
            snapshot.children.clear();
        }
        EventKind::Enrichment
        | EventKind::Working
        | EventKind::WaitingInput
        | EventKind::WaitingApproval => {}
    }
    if authoritative_activity(&event.kind, event.evidence.channel) {
        if snapshot.activity != prior_activity {
            snapshot.state_started_at = event.received_at;
        }
        snapshot.last_state_asserted_at = Some(event.received_at);
        snapshot.activity_confirmation = ActivityConfirmation::Live;
    }
    if let Some(id) = &event.provider_session_id {
        let prior = snapshot.provider_session.as_ref();
        let is_new = prior.is_none_or(|session| session.id != *id);
        if is_new {
            snapshot.usage = None;
            snapshot.children.clear();
        }
        snapshot.provider_session = Some(ProviderSession {
            id: id.clone(),
            name: event.provider_session_name.clone().or_else(|| {
                snapshot
                    .provider_session
                    .as_ref()
                    .filter(|session| session.id == *id)
                    .and_then(|session| session.name.clone())
            }),
            generation: if is_new {
                prior.map_or(1, |session| session.generation.saturating_add(1))
            } else {
                prior.map_or(1, |session| session.generation)
            },
            start_reason: event.provider_session_start_reason.clone().or_else(|| {
                prior
                    .filter(|session| session.id == *id)
                    .and_then(|session| session.start_reason.clone())
            }),
        });
    }
    if let Some(metadata) = &event.provider_metadata {
        let current = snapshot.provider_metadata.get_or_insert_default();
        if metadata.model.is_some() {
            current.model.clone_from(&metadata.model);
        }
        if metadata.effort.is_some() {
            current.effort.clone_from(&metadata.effort);
        }
        if metadata.permission_mode.is_some() {
            current
                .permission_mode
                .clone_from(&metadata.permission_mode);
        }
        if metadata.current_turn_id.is_some() {
            current
                .current_turn_id
                .clone_from(&metadata.current_turn_id);
        }
    }
    if let Some(turn_id) = &event.turn_id {
        snapshot
            .provider_metadata
            .get_or_insert_default()
            .current_turn_id = Some(turn_id.clone());
    }
    if event.usage.is_some() {
        snapshot.usage.clone_from(&event.usage);
    }
    record_source_order(snapshot, &event.evidence);
    snapshot.last_evidence = Some(event.evidence.clone());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{
        Capabilities, InvocationId, ProcessMetadata, ProviderMetadata, PublicStatus,
        StatusReasonSource, Usage,
    };
    use chrono::TimeZone;

    fn at(seconds: i64) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2000, 1, 1, 0, 0, 0).unwrap() + Duration::seconds(seconds)
    }

    fn snapshot() -> InvocationSnapshot {
        InvocationSnapshot {
            schema_version: crate::SCHEMA_VERSION,
            revision: 1,
            invocation_id: InvocationId(uuid::Uuid::nil()),
            provider: "claude".into(),
            executable: "claude".into(),
            args: vec![],
            cwd: "/work".into(),
            process: ProcessMetadata::default(),
            created_at: at(0),
            updated_at: at(0),
            lifecycle: Lifecycle::Alive,
            activity: Activity::Idle,
            state_started_at: at(0),
            last_state_asserted_at: Some(at(0)),
            activity_confirmation: ActivityConfirmation::Live,
            last_evidence: None,
            source_ordering: vec![],
            current_tool_activity: None,
            status: PublicStatus::Idle,
            provider_session: None,
            provider_metadata: None,
            usage: None,
            repository: None,
            multiplexer: None,
            capabilities: Capabilities::default(),
            turn_generation: 0,
            completed_generation: None,
            children: Vec::new(),
        }
    }

    fn event(kind: EventKind) -> NormalizedEvent {
        NormalizedEvent {
            schema_version: 1,
            event_id: "event".into(),
            invocation_id: InvocationId(uuid::Uuid::nil()),
            provider_event_id: None,
            provider: "claude".into(),
            observed_at: at(1),
            received_at: at(1),
            evidence: EventEvidence::managed_hook(1),
            kind,
            provider_session_id: None,
            provider_session_name: None,
            provider_session_start_reason: None,
            provider_metadata: None,
            usage: None,
            turn_id: None,
            tool_activity: None,
            child_agent: None,
        }
    }

    fn context(summary: &str) -> StatusReasonContext {
        StatusReasonContext {
            summary: summary.into(),
            source: StatusReasonSource::Question,
        }
    }

    fn reason(kind: EventKind, summary: &str) -> CurrentStatusReason {
        CurrentStatusReason {
            kind,
            context: context(summary),
        }
    }

    /// Minimal stand-in for the storage row: applies events in order and
    /// resolves reason effects the way the transaction helper does.
    struct State {
        snapshot: InvocationSnapshot,
        reason: Option<CurrentStatusReason>,
    }

    impl State {
        fn new() -> Self {
            Self {
                snapshot: snapshot(),
                reason: None,
            }
        }

        fn prior(&self) -> Prior<'_> {
            Prior {
                snapshot: &self.snapshot,
                reason: self.reason.as_ref(),
            }
        }

        fn apply(
            &mut self,
            event: &NormalizedEvent,
            context: Option<&StatusReasonContext>,
        ) -> Transition {
            let transition = apply_event(self.prior(), event, context).unwrap();
            self.commit(&transition);
            transition
        }

        fn commit(&mut self, transition: &Transition) {
            self.reason = transition.reason.resolve(self.reason.as_ref()).cloned();
            self.snapshot = transition.snapshot.clone();
        }
    }

    #[test]
    fn stale_source_order_is_suppressed() {
        let mut state = State::new();
        let mut first = event(EventKind::Working);
        first.evidence = EventEvidence {
            channel: EvidenceChannel::SideChannel,
            trust: EvidenceTrust::LocalObservation,
            collector_revision: Some(1),
            collector_instance_id: Some("a".into()),
            source_sequence: Some(2),
        };
        assert!(!state.apply(&first, None).suppressed);
        let mut late = first.clone();
        late.kind = EventKind::Idle;
        late.evidence.source_sequence = Some(2);
        let transition = state.apply(&late, None);
        assert!(transition.suppressed);
        assert_eq!(transition.reason, ReasonEffect::Keep);
        assert_eq!(transition.snapshot.activity, Activity::Working);
        let mut other_instance = late.clone();
        other_instance.evidence.collector_instance_id = Some("b".into());
        assert!(!state.apply(&other_instance, None).suppressed);
    }

    #[test]
    fn stale_session_and_turn_events_cannot_regress_state() {
        let mut state = State::new();
        let mut start = event(EventKind::ProviderSessionStarted);
        start.provider_session_id = Some("b".into());
        start.turn_id = Some("turn-2".into());
        state.apply(&start, None);

        let mut stale_session = event(EventKind::WaitingApproval);
        stale_session.provider_session_id = Some("a".into());
        stale_session.provider_metadata = Some(ProviderMetadata {
            permission_mode: Some("auto".into()),
            ..Default::default()
        });
        let transition = state.apply(&stale_session, Some(&context("Approve")));
        assert!(transition.suppressed);
        assert_eq!(transition.reason, ReasonEffect::Keep);

        let mut stale_turn = event(EventKind::WaitingApproval);
        stale_turn.turn_id = Some("turn-1".into());
        assert!(state.apply(&stale_turn, None).suppressed);

        assert_eq!(state.snapshot.activity, Activity::Idle);
        assert_eq!(state.snapshot.provider_session.as_ref().unwrap().id, "b");
        assert_eq!(
            state
                .snapshot
                .provider_metadata
                .as_ref()
                .and_then(|m| m.permission_mode.as_deref()),
            None
        );

        let mut next_turn = event(EventKind::NewTurn);
        next_turn.turn_id = Some("turn-3".into());
        assert!(!state.apply(&next_turn, None).suppressed);
        let mut restart = event(EventKind::ProviderSessionStarted);
        restart.provider_session_id = Some("c".into());
        assert!(!state.apply(&restart, None).suppressed);
    }

    #[test]
    fn duplicate_stop_cause_and_late_work_after_terminal_are_suppressed() {
        let mut state = State::new();
        state.apply(&event(EventKind::NewTurn), None);
        let completed = state.apply(&event(EventKind::Completed), Some(&context("Done")));
        assert!(!completed.suppressed);
        assert_eq!(
            completed.reason,
            ReasonEffect::Set(reason(EventKind::Completed, "Done"))
        );
        let mut tool = event(EventKind::Working);
        tool.tool_activity = Some(ToolActivityUpdate {
            phase: ToolActivityPhase::Start,
            label: "shell".into(),
            correlation_id: None,
            detail: None,
        });
        for late in [
            tool,
            event(EventKind::WaitingInput),
            event(EventKind::WaitingApproval),
            event(EventKind::Completed),
            event(EventKind::Failed),
            event(EventKind::Interrupted),
        ] {
            let transition = state.apply(&late, Some(&context("Late")));
            assert!(transition.suppressed, "{:?} was not suppressed", late.kind);
            assert_eq!(transition.reason, ReasonEffect::Keep);
            assert!(transition.snapshot.current_tool_activity.is_none());
        }
        assert_eq!(state.snapshot.activity, Activity::Stopped);
        assert_eq!(state.reason, Some(reason(EventKind::Completed, "Done")));
        let idle = state.apply(&event(EventKind::Idle), None);
        assert!(!idle.suppressed);
        assert_eq!(idle.reason, ReasonEffect::Clear);
    }

    #[test]
    fn exited_invocations_accept_only_enrichment() {
        for lifecycle in [Lifecycle::Exited, Lifecycle::Lost] {
            let mut state = State::new();
            state.snapshot.lifecycle = lifecycle;
            state.reason = Some(reason(EventKind::Completed, "Done"));
            let mut working = event(EventKind::Working);
            working.tool_activity = Some(ToolActivityUpdate {
                phase: ToolActivityPhase::Start,
                label: "shell".into(),
                correlation_id: None,
                detail: None,
            });
            working.provider_metadata = Some(ProviderMetadata {
                model: Some("late-model".into()),
                ..Default::default()
            });
            let transition = state.apply(&working, None);
            assert!(!transition.suppressed);
            assert_eq!(transition.reason, ReasonEffect::Keep);
            assert_eq!(transition.snapshot.activity, Activity::Idle);
            assert_eq!(transition.snapshot.lifecycle, lifecycle);
            assert_eq!(transition.snapshot.status, PublicStatus::Stopped);
            assert!(transition.snapshot.current_tool_activity.is_none());
            assert_eq!(
                transition
                    .snapshot
                    .provider_metadata
                    .unwrap()
                    .model
                    .as_deref(),
                Some("late-model")
            );
        }
    }

    #[test]
    fn unbounded_inputs_are_rejected() {
        let prior = snapshot();
        let prior = Prior {
            snapshot: &prior,
            reason: None,
        };
        let long = "x".repeat(STATUS_REASON_MAX_CHARS + 1);
        for summary in ["", "line\nbreak", long.as_str()] {
            assert!(
                apply_event(
                    prior,
                    &event(EventKind::WaitingInput),
                    Some(&context(summary))
                )
                .is_err()
            );
        }
        let mut forged = event(EventKind::Working);
        forged.evidence.trust = EvidenceTrust::LocalObservation;
        assert!(apply_event(prior, &forged, None).is_err());
        let mut tool = event(EventKind::Working);
        tool.tool_activity = Some(ToolActivityUpdate {
            phase: ToolActivityPhase::Start,
            label: String::new(),
            correlation_id: None,
            detail: None,
        });
        assert!(apply_event(prior, &tool, None).is_err());
    }

    #[test]
    fn reason_effects_follow_the_effective_kind() {
        let waiting = reason(EventKind::WaitingInput, "Choose");
        let completed = reason(EventKind::Completed, "Done");
        let interrupted = reason(EventKind::Interrupted, "Stop");
        let cases = [
            (
                EventKind::WaitingInput,
                Some("Choose"),
                None,
                ReasonEffect::Set(waiting.clone()),
            ),
            (
                EventKind::WaitingApproval,
                None,
                Some(&waiting),
                ReasonEffect::Clear,
            ),
            (
                EventKind::Working,
                None,
                Some(&waiting),
                ReasonEffect::Clear,
            ),
            (EventKind::Idle, None, Some(&completed), ReasonEffect::Clear),
            (
                EventKind::NewTurn,
                None,
                Some(&completed),
                ReasonEffect::Clear,
            ),
            (
                EventKind::ProviderSessionStarted,
                None,
                Some(&completed),
                ReasonEffect::Clear,
            ),
            (
                EventKind::SessionEnded,
                None,
                Some(&waiting),
                ReasonEffect::Clear,
            ),
            (
                EventKind::SessionEnded,
                None,
                Some(&interrupted),
                ReasonEffect::Clear,
            ),
            (
                EventKind::SessionEnded,
                None,
                Some(&completed),
                ReasonEffect::Keep,
            ),
            (EventKind::SessionEnded, None, None, ReasonEffect::Keep),
            (
                EventKind::ProviderSessionEnded,
                None,
                Some(&waiting),
                ReasonEffect::Keep,
            ),
            (
                EventKind::Enrichment,
                Some("Ignored"),
                Some(&waiting),
                ReasonEffect::Keep,
            ),
        ];
        for (kind, summary, prior_reason, expected) in cases {
            let prior = snapshot();
            let transition = apply_event(
                Prior {
                    snapshot: &prior,
                    reason: prior_reason,
                },
                &event(kind.clone()),
                summary.map(context).as_ref(),
            )
            .unwrap();
            assert_eq!(
                transition.reason, expected,
                "{kind:?} with {prior_reason:?}"
            );
        }
    }

    #[test]
    fn provider_sessions_are_ordered_and_do_not_end_the_wrapper() {
        let mut state = State::new();
        let mut first = event(EventKind::ProviderSessionStarted);
        first.provider_session_id = Some("a".into());
        first.provider_session_start_reason = Some("startup".into());
        first.usage = Some(Usage {
            input_tokens: Some(1),
            ..Default::default()
        });
        state.apply(&first, None);
        assert_eq!(
            state.snapshot.provider_session.as_ref().unwrap().generation,
            1
        );
        assert!(state.snapshot.usage.is_some());

        let mut second = event(EventKind::ProviderSessionStarted);
        second.provider_session_id = Some("b".into());
        second.provider_session_start_reason = Some("clear".into());
        state.apply(&second, None);
        let session = state.snapshot.provider_session.clone().unwrap();
        assert_eq!(session.generation, 2);
        assert_eq!(session.start_reason.as_deref(), Some("clear"));
        assert!(state.snapshot.usage.is_none());

        let mut ended = event(EventKind::ProviderSessionEnded);
        ended.provider_session_id = Some("b".into());
        state.apply(&ended, None);
        assert_eq!(state.snapshot.lifecycle, Lifecycle::Alive);
        assert_eq!(state.snapshot.activity, Activity::Idle);
        assert_eq!(state.snapshot.provider_session.unwrap().generation, 2);
    }

    #[test]
    fn provider_session_name_updates_and_is_preserved() {
        let mut state = State::new();
        for (name, expected) in [
            (Some("First name"), "First name"),
            (None, "First name"),
            (Some("Second name"), "Second name"),
        ] {
            let mut working = event(EventKind::Working);
            working.provider_session_id = Some("provider-session".into());
            working.provider_session_name = name.map(Into::into);
            state.apply(&working, None);
            assert_eq!(
                state
                    .snapshot
                    .provider_session
                    .as_ref()
                    .unwrap()
                    .name
                    .as_deref(),
                Some(expected)
            );
        }
    }

    #[test]
    fn attention_reason_replaces_survives_enrichment_and_clears_on_work() {
        let mut state = State::new();
        state.apply(&event(EventKind::WaitingInput), Some(&context("First")));
        state.apply(&event(EventKind::WaitingInput), Some(&context("Second")));
        assert_eq!(
            state.reason,
            Some(reason(EventKind::WaitingInput, "Second"))
        );
        state.apply(&event(EventKind::Enrichment), None);
        let view = project_public(&state.snapshot, state.reason.as_ref());
        assert_eq!(view.reason.unwrap().summary, "Second");
        state.apply(&event(EventKind::Working), None);
        assert!(state.reason.is_none());
    }

    #[test]
    fn repeated_terminal_is_suppressed_and_keeps_the_first_reason() {
        let mut state = State::new();
        state.apply(&event(EventKind::NewTurn), None);
        state.apply(
            &event(EventKind::WaitingApproval),
            Some(&context("Approve")),
        );
        let first = state.apply(&event(EventKind::Completed), None);
        assert!(!first.suppressed);
        assert_eq!(first.reason, ReasonEffect::Clear);
        let second = state.apply(&event(EventKind::Completed), Some(&context("Late")));
        assert!(second.suppressed);
        assert_eq!(second.reason, ReasonEffect::Keep);
        assert!(state.reason.is_none());
    }

    #[test]
    fn artifact_usage_replaces_atomically_and_unchanged_usage_is_not_a_change() {
        let mut state = State::new();
        state.snapshot.usage = Some(Usage {
            input_tokens: Some(1),
            output_tokens: Some(1),
            context_tokens: Some(1),
            context_window_percent: Some(1),
        });
        let mut artifact = event(EventKind::Working);
        artifact.evidence = EventEvidence::local(EvidenceChannel::ProviderArtifact);
        artifact.usage = Some(Usage {
            input_tokens: Some(100),
            output_tokens: Some(20),
            context_tokens: None,
            context_window_percent: None,
        });
        let transition = state.apply(&artifact, None);
        assert_eq!(transition.snapshot.activity, Activity::Idle);
        assert_eq!(transition.snapshot.usage, artifact.usage);
        let prior_view = project_public(&state.snapshot, None);
        let mut again = apply_event(state.prior(), &artifact, None)
            .unwrap()
            .snapshot;
        let (_, changed) = finalize(&prior_view, &mut again, None, at(99));
        assert!(changed.is_empty());
        assert_eq!(again.updated_at, state.snapshot.updated_at);
    }

    #[test]
    fn mark_lost_keeps_only_successful_or_failed_outcomes() {
        for (prior_reason, expected) in [
            (
                Some(reason(EventKind::Completed, "Done")),
                ReasonEffect::Keep,
            ),
            (Some(reason(EventKind::Failed, "Oops")), ReasonEffect::Keep),
            (
                Some(reason(EventKind::Interrupted, "Stop")),
                ReasonEffect::Clear,
            ),
            (
                Some(reason(EventKind::WaitingInput, "Choose")),
                ReasonEffect::Clear,
            ),
            (None, ReasonEffect::Keep),
        ] {
            let mut prior = snapshot();
            prior.current_tool_activity = Some(CurrentToolActivity {
                label: "shell".into(),
                correlation_id: None,
                detail: None,
                started_at: at(0),
                last_observed_at: at(0),
            });
            let transition = mark_lost(Prior {
                snapshot: &prior,
                reason: prior_reason.as_ref(),
            });
            assert_eq!(transition.reason, expected);
            assert_eq!(transition.snapshot.lifecycle, Lifecycle::Lost);
            assert_eq!(transition.snapshot.status, PublicStatus::Stopped);
            assert!(transition.snapshot.current_tool_activity.is_none());
            assert_eq!(
                transition.snapshot.last_evidence.unwrap().channel,
                EvidenceChannel::ProcessObservation
            );
        }
    }

    #[test]
    fn stale_working_expires_only_after_the_threshold() {
        let mut prior = snapshot();
        prior.activity = Activity::Working;
        prior.last_state_asserted_at = Some(at(60));
        let view = Prior {
            snapshot: &prior,
            reason: None,
        };
        let threshold = at(60) + Duration::minutes(STALE_WORKING_MINUTES);
        assert!(expire_stale_working(view, threshold - Duration::seconds(1)).is_none());
        let transition = expire_stale_working(view, threshold).unwrap();
        assert_eq!(transition.snapshot.activity, Activity::Unknown);
        assert_eq!(transition.snapshot.state_started_at, threshold);
        assert_eq!(transition.snapshot.status, PublicStatus::Idle);
        assert_eq!(transition.reason, ReasonEffect::Clear);

        let mut waiting = prior.clone();
        waiting.activity = Activity::WaitingInput;
        assert!(
            expire_stale_working(
                Prior {
                    snapshot: &waiting,
                    reason: None
                },
                threshold
            )
            .is_none()
        );
        let mut exited = prior.clone();
        exited.lifecycle = Lifecycle::Exited;
        assert!(
            expire_stale_working(
                Prior {
                    snapshot: &exited,
                    reason: None
                },
                threshold
            )
            .is_none()
        );
        let mut unasserted = prior;
        unasserted.last_state_asserted_at = None;
        unasserted.state_started_at = at(0);
        assert!(
            expire_stale_working(
                Prior {
                    snapshot: &unasserted,
                    reason: None
                },
                at(0) + Duration::minutes(STALE_WORKING_MINUTES)
            )
            .is_some()
        );
    }

    fn child_state(agent_id: &str, activity: ChildActivity, updated_at: i64) -> ChildAgentState {
        ChildAgentState {
            agent_id: agent_id.into(),
            agent_type: "Explore".into(),
            activity,
            reason: match activity {
                ChildActivity::Running => None,
                ChildActivity::Blocked => child_reason(Some(PublicReasonKind::Approval), None),
                ChildActivity::Stopped => child_reason(Some(PublicReasonKind::Completed), None),
            },
            started_at: at(0),
            updated_at: at(updated_at),
        }
    }

    #[test]
    fn stale_running_children_expire_and_others_are_kept() {
        let mut prior = snapshot();
        prior.children = vec![
            child_state("stale", ChildActivity::Running, 0),
            child_state("blocked", ChildActivity::Blocked, 0),
            child_state("stopped", ChildActivity::Stopped, 0),
            child_state("fresh", ChildActivity::Running, 60),
        ];
        let view = Prior {
            snapshot: &prior,
            reason: None,
        };
        let threshold = at(0) + Duration::minutes(STALE_WORKING_MINUTES);
        assert!(!is_stale(&prior, threshold - Duration::seconds(1)));
        assert!(expire_stale_working(view, threshold - Duration::seconds(1)).is_none());
        assert!(is_stale(&prior, threshold));
        let transition = expire_stale_working(view, threshold).unwrap();
        let kept: Vec<_> = transition
            .snapshot
            .children
            .iter()
            .map(|child| child.agent_id.as_str())
            .collect();
        assert_eq!(kept, ["blocked", "stopped", "fresh"]);
        assert_eq!(transition.snapshot.children[0], prior.children[1]);
        assert_eq!(transition.snapshot.children[1], prior.children[2]);
        assert_eq!(transition.reason, ReasonEffect::Keep);
    }

    #[test]
    fn child_event_resets_the_child_stale_clock() {
        let mut state = State::new();
        state.apply(&child_event(EventKind::NewTurn, "a"), None);
        let mut working = child_event(EventKind::Working, "a");
        working.received_at = at(600);
        state.apply(&working, None);
        let threshold = at(1) + Duration::minutes(STALE_WORKING_MINUTES);
        assert!(expire_stale_working(state.prior(), threshold).is_none());
        let transition = expire_stale_working(
            state.prior(),
            at(600) + Duration::minutes(STALE_WORKING_MINUTES),
        )
        .unwrap();
        assert!(transition.snapshot.children.is_empty());
    }

    #[test]
    fn stale_child_under_stopped_root_leaves_root_intact() {
        let mut state = State::new();
        state.apply(&event(EventKind::NewTurn), None);
        state.apply(&child_event(EventKind::NewTurn, "a"), None);
        state.apply(&event(EventKind::Completed), Some(&context("Done")));
        let root = state.snapshot.clone();
        let now = at(1) + Duration::minutes(STALE_WORKING_MINUTES);
        let transition = expire_stale_working(state.prior(), now).unwrap();
        assert_eq!(transition.reason, ReasonEffect::Keep);
        state.commit(&transition);
        assert!(state.snapshot.children.is_empty());
        assert_eq!(state.snapshot.activity, root.activity);
        assert_eq!(state.snapshot.activity, Activity::Stopped);
        assert_eq!(state.snapshot.status, PublicStatus::Stopped);
        assert_eq!(state.snapshot.state_started_at, root.state_started_at);
        assert_eq!(state.reason, Some(reason(EventKind::Completed, "Done")));
        let prior_view = project_public(&root, state.reason.as_ref());
        let mut next = state.snapshot.clone();
        let (view, changed) = finalize(&prior_view, &mut next, state.reason.as_ref(), now);
        assert!(view.children.is_none());
        assert_eq!(
            changed,
            BTreeSet::from([PublicField::UpdatedAt, PublicField::Children])
        );
    }

    #[test]
    fn stale_root_and_child_expire_in_one_transition() {
        let mut prior = snapshot();
        prior.activity = Activity::Working;
        prior.status = PublicStatus::Running;
        prior.last_state_asserted_at = Some(at(0));
        prior.children = vec![child_state("a", ChildActivity::Running, 0)];
        let now = at(0) + Duration::minutes(STALE_WORKING_MINUTES);
        let transition = expire_stale_working(
            Prior {
                snapshot: &prior,
                reason: None,
            },
            now,
        )
        .unwrap();
        assert_eq!(transition.snapshot.activity, Activity::Unknown);
        assert!(transition.snapshot.children.is_empty());
        assert_eq!(transition.reason, ReasonEffect::Clear);
        let prior_view = project_public(&prior, None);
        let mut next = transition.snapshot;
        let (_, changed) = finalize(&prior_view, &mut next, None, now);
        assert!(changed.contains(&PublicField::Status));
        assert!(changed.contains(&PublicField::Children));
    }

    #[test]
    fn local_mutation_exit_keeps_terminal_outcomes() {
        let mut prior = snapshot();
        prior.current_tool_activity = Some(CurrentToolActivity {
            label: "shell".into(),
            correlation_id: None,
            detail: None,
            started_at: at(0),
            last_observed_at: at(0),
        });
        for (kind, expected) in [
            (EventKind::Completed, ReasonEffect::Keep),
            (EventKind::Failed, ReasonEffect::Keep),
            (EventKind::Interrupted, ReasonEffect::Keep),
            (EventKind::WaitingApproval, ReasonEffect::Clear),
        ] {
            let prior_reason = reason(kind, "Reason");
            let exit = local_mutation(
                Prior {
                    snapshot: &prior,
                    reason: Some(&prior_reason),
                },
                true,
                |snapshot| snapshot.lifecycle = Lifecycle::Exited,
            );
            assert_eq!(exit.reason, expected);
            assert_eq!(exit.snapshot.status, PublicStatus::Stopped);
            assert!(exit.snapshot.current_tool_activity.is_none());
        }
        let waiting = reason(EventKind::WaitingInput, "Choose");
        let bind = local_mutation(
            Prior {
                snapshot: &prior,
                reason: Some(&waiting),
            },
            false,
            |snapshot| snapshot.process.child_pid = Some(7),
        );
        assert_eq!(bind.reason, ReasonEffect::Keep);
        assert!(bind.snapshot.current_tool_activity.is_some());
        assert_eq!(
            bind.snapshot.last_evidence.unwrap().channel,
            EvidenceChannel::ProcessObservation
        );
    }

    #[test]
    fn finalize_bumps_updated_at_only_when_the_view_differs() {
        let prior = snapshot();
        let prior_view = project_public(&prior, None);
        let mut private_only = prior.clone();
        private_only.activity_confirmation = ActivityConfirmation::RestoredUnconfirmed;
        let (view, changed) = finalize(&prior_view, &mut private_only, None, at(50));
        assert!(changed.is_empty());
        assert_eq!(view, prior_view);
        assert_eq!(private_only.updated_at, at(0));

        let mut working = prior.clone();
        working.activity = Activity::Working;
        let (view, changed) = finalize(&prior_view, &mut working, None, at(50));
        assert_eq!(
            changed,
            BTreeSet::from([PublicField::Status, PublicField::UpdatedAt])
        );
        assert_eq!(working.updated_at, at(50));
        assert_eq!(view.updated_at, at(50));

        let mut reason_only = prior;
        reason_only.activity = Activity::WaitingInput;
        let waiting = reason(EventKind::WaitingInput, "Choose");
        let (view, changed) = finalize(&prior_view, &mut reason_only, Some(&waiting), at(60));
        assert!(changed.contains(&PublicField::Reason));
        assert_eq!(view.reason.unwrap().summary, "Choose");
    }

    fn child_event(kind: EventKind, agent_id: &str) -> NormalizedEvent {
        let mut value = event(kind);
        value.child_agent = Some(ChildAgentRef {
            agent_id: agent_id.into(),
            agent_type: "Explore".into(),
        });
        value
    }

    fn with_tool(mut value: NormalizedEvent, label: &str) -> NormalizedEvent {
        value.tool_activity = Some(ToolActivityUpdate {
            phase: ToolActivityPhase::Start,
            label: label.into(),
            correlation_id: Some("toolu_1".into()),
            detail: Some("PRIVATE_DETAIL".into()),
        });
        value
    }

    fn child_reason(
        kind: Option<PublicReasonKind>,
        summary: Option<&str>,
    ) -> Option<PublicChildAgentReason> {
        Some(PublicChildAgentReason {
            kind,
            summary: summary.map(Into::into),
        })
    }

    #[test]
    fn child_kinds_map_to_child_activity_and_reason() {
        let cases = [
            (EventKind::NewTurn, ChildActivity::Running, None),
            (EventKind::Working, ChildActivity::Running, None),
            (
                EventKind::WaitingInput,
                ChildActivity::Blocked,
                Some(PublicReasonKind::Input),
            ),
            (
                EventKind::WaitingApproval,
                ChildActivity::Blocked,
                Some(PublicReasonKind::Approval),
            ),
            (
                EventKind::Completed,
                ChildActivity::Stopped,
                Some(PublicReasonKind::Completed),
            ),
            (
                EventKind::Failed,
                ChildActivity::Stopped,
                Some(PublicReasonKind::Failed),
            ),
            (
                EventKind::Interrupted,
                ChildActivity::Stopped,
                Some(PublicReasonKind::Failed),
            ),
        ];
        for (kind, activity, reason_kind) in cases {
            let mut state = State::new();
            state.apply(&with_tool(child_event(kind.clone(), "a"), "bash"), None);
            let child = &state.snapshot.children[0];
            assert_eq!(child.activity, activity, "{kind:?}");
            assert_eq!(child.reason, child_reason(reason_kind, Some("bash")));
        }
        for kind in [
            EventKind::Idle,
            EventKind::ProviderSessionStarted,
            EventKind::ProviderSessionEnded,
            EventKind::SessionEnded,
            EventKind::Enrichment,
        ] {
            let mut state = State::new();
            state.apply(&child_event(kind.clone(), "a"), None);
            assert!(state.snapshot.children.is_empty(), "{kind:?}");
            assert_eq!(state.snapshot.lifecycle, Lifecycle::Alive);
        }
    }

    #[test]
    fn child_summary_follows_tool_labels_and_clears_on_stop() {
        let mut state = State::new();
        state.apply(&child_event(EventKind::NewTurn, "a"), None);
        assert_eq!(state.snapshot.children[0].activity, ChildActivity::Running);
        assert_eq!(state.snapshot.children[0].reason, None);
        state.apply(
            &with_tool(child_event(EventKind::Working, "a"), "bash"),
            None,
        );
        assert_eq!(
            state.snapshot.children[0].reason,
            child_reason(None, Some("bash"))
        );
        state.apply(&child_event(EventKind::WaitingApproval, "a"), None);
        assert_eq!(
            state.snapshot.children[0].reason,
            child_reason(Some(PublicReasonKind::Approval), Some("bash"))
        );
        state.apply(&child_event(EventKind::Completed, "a"), None);
        let child = &state.snapshot.children[0];
        assert_eq!(child.activity, ChildActivity::Stopped);
        assert_eq!(
            child.reason,
            child_reason(Some(PublicReasonKind::Completed), None)
        );
        assert_eq!(state.snapshot.status, PublicStatus::Idle);
    }

    #[test]
    fn unknown_child_is_created_from_its_first_event() {
        let mut state = State::new();
        let mut working = with_tool(child_event(EventKind::Working, "late-start"), "read");
        working.received_at = at(5);
        state.apply(&working, None);
        let child = &state.snapshot.children[0];
        assert_eq!(child.agent_id, "late-start");
        assert_eq!(child.agent_type, "Explore");
        assert_eq!(child.activity, ChildActivity::Running);
        assert_eq!((child.started_at, child.updated_at), (at(5), at(5)));
        let mut start = child_event(EventKind::NewTurn, "late-start");
        start.received_at = at(6);
        state.apply(&start, None);
        assert_eq!(state.snapshot.children.len(), 1);
        assert_eq!(state.snapshot.children[0].started_at, at(5));
        assert_eq!(state.snapshot.children[0].updated_at, at(6));
    }

    #[test]
    fn full_child_list_evicts_the_oldest_stopped_child_first() {
        let mut state = State::new();
        for index in 0..CHILD_AGENTS_MAX {
            let mut start = child_event(EventKind::NewTurn, &format!("child-{index:02}"));
            start.received_at = at(i64::try_from(index).unwrap());
            state.apply(&start, None);
        }
        state.apply(&child_event(EventKind::NewTurn, "overflow"), None);
        assert_eq!(state.snapshot.children.len(), CHILD_AGENTS_MAX);
        assert!(
            state
                .snapshot
                .children
                .iter()
                .all(|child| child.agent_id != "overflow")
        );
        for id in ["child-07", "child-03"] {
            state.apply(&child_event(EventKind::Completed, id), None);
        }
        state.apply(&child_event(EventKind::NewTurn, "overflow"), None);
        let ids: Vec<&str> = state
            .snapshot
            .children
            .iter()
            .map(|child| child.agent_id.as_str())
            .collect();
        assert_eq!(ids.len(), CHILD_AGENTS_MAX);
        assert!(ids.contains(&"overflow"));
        assert!(!ids.contains(&"child-03"));
        assert!(ids.contains(&"child-07"));
    }

    #[test]
    fn child_approval_after_root_stop_leaves_root_state_intact() {
        let mut state = State::new();
        state.apply(&event(EventKind::NewTurn), None);
        state.apply(&child_event(EventKind::NewTurn, "a"), None);
        state.apply(&event(EventKind::Completed), Some(&context("Done")));
        let root = state.snapshot.clone();
        let transition = state.apply(
            &with_tool(child_event(EventKind::WaitingApproval, "a"), "bash"),
            Some(&context("Ignored")),
        );
        assert!(!transition.suppressed);
        assert_eq!(transition.reason, ReasonEffect::Keep);
        assert_eq!(state.snapshot.activity, Activity::Stopped);
        assert_eq!(state.snapshot.status, PublicStatus::Stopped);
        assert_eq!(state.reason, Some(reason(EventKind::Completed, "Done")));
        assert_eq!(
            state.snapshot.completed_generation,
            root.completed_generation
        );
        let child = &state.snapshot.children[0];
        assert_eq!(child.activity, ChildActivity::Blocked);
        assert_eq!(
            child.reason,
            child_reason(Some(PublicReasonKind::Approval), Some("bash"))
        );
        let view = project_public(&state.snapshot, state.reason.as_ref());
        assert_eq!(view.status, PublicStatus::Stopped);
        assert_eq!(view.reason.unwrap().summary, "Done");
        let prior_view = project_public(&root, state.reason.as_ref());
        let mut next = state.snapshot.clone();
        let (_, changed) = finalize(&prior_view, &mut next, state.reason.as_ref(), at(9));
        assert_eq!(
            changed,
            BTreeSet::from([PublicField::UpdatedAt, PublicField::Children])
        );
    }

    #[test]
    fn child_events_obey_session_and_source_order_guards() {
        let mut state = State::new();
        let mut start = event(EventKind::ProviderSessionStarted);
        start.provider_session_id = Some("current".into());
        state.apply(&start, None);
        let mut older = child_event(EventKind::Working, "a");
        older.provider_session_id = Some("older".into());
        let transition = state.apply(&older, None);
        assert!(transition.suppressed);
        assert!(state.snapshot.children.is_empty());
        assert_eq!(
            state.snapshot.provider_session.as_ref().unwrap().id,
            "current"
        );

        let mut ordered = child_event(EventKind::Working, "a");
        ordered.provider_session_id = Some("current".into());
        ordered.evidence = EventEvidence {
            channel: EvidenceChannel::SideChannel,
            trust: EvidenceTrust::LocalObservation,
            collector_revision: Some(1),
            collector_instance_id: Some("side".into()),
            source_sequence: Some(4),
        };
        assert!(!state.apply(&ordered, None).suppressed);
        let mut late = ordered.clone();
        late.kind = EventKind::Completed;
        assert!(state.apply(&late, None).suppressed);
        assert_eq!(state.snapshot.children[0].activity, ChildActivity::Running);
    }

    #[test]
    fn child_state_clears_at_root_retention_boundaries() {
        let retained = || {
            let mut state = State::new();
            let mut start = event(EventKind::ProviderSessionStarted);
            start.provider_session_id = Some("s".into());
            state.apply(&start, None);
            state.apply(&child_event(EventKind::NewTurn, "a"), None);
            assert_eq!(state.snapshot.children.len(), 1);
            state
        };
        let mut state = retained();
        state.apply(&event(EventKind::NewTurn), None);
        assert!(state.snapshot.children.is_empty());
        assert!(project_public(&state.snapshot, None).children.is_none());

        let mut state = retained();
        let mut next = event(EventKind::ProviderSessionStarted);
        next.provider_session_id = Some("t".into());
        state.apply(&next, None);
        assert!(state.snapshot.children.is_empty());

        let mut state = retained();
        let mut same = event(EventKind::Working);
        same.provider_session_id = Some("s".into());
        state.apply(&same, None);
        assert_eq!(state.snapshot.children.len(), 1);

        let mut state = retained();
        state.apply(&event(EventKind::SessionEnded), None);
        assert!(state.snapshot.children.is_empty());
        state.apply(&child_event(EventKind::Working, "a"), None);
        assert!(state.snapshot.children.is_empty());

        let state = retained();
        let lost = mark_lost(state.prior());
        assert!(lost.snapshot.children.is_empty());
        assert_eq!(lost.snapshot.status, PublicStatus::Stopped);

        let state = retained();
        let exit = local_mutation(state.prior(), true, |snapshot| {
            snapshot.lifecycle = Lifecycle::Exited;
        });
        assert!(exit.snapshot.children.is_empty());
        assert_eq!(exit.snapshot.status, PublicStatus::Stopped);

        let state = retained();
        let bind = local_mutation(state.prior(), false, |snapshot| {
            snapshot.process.child_pid = Some(7);
        });
        assert_eq!(bind.snapshot.children.len(), 1);
    }

    #[test]
    fn child_events_never_touch_root_state() {
        let mut state = State::new();
        let mut turn = event(EventKind::NewTurn);
        turn.provider_session_id = Some("s".into());
        turn.provider_session_name = Some("Root".into());
        turn.turn_id = Some("turn-1".into());
        turn.provider_metadata = Some(ProviderMetadata {
            model: Some("root-model".into()),
            ..Default::default()
        });
        turn.usage = Some(Usage {
            input_tokens: Some(1),
            ..Default::default()
        });
        state.apply(&with_tool(turn, "root-tool"), None);
        state.apply(&with_tool(event(EventKind::Working), "root-tool"), None);
        let prior = state.snapshot.clone();

        let mut child = with_tool(child_event(EventKind::Working, "a"), "child-tool");
        child.tool_activity.as_mut().unwrap().phase = ToolActivityPhase::Finish;
        child.provider_session_id = Some("s".into());
        child.provider_session_name = Some("Child".into());
        child.provider_session_start_reason = Some("startup".into());
        child.turn_id = Some("turn-child".into());
        child.provider_metadata = Some(ProviderMetadata {
            model: Some("child-model".into()),
            permission_mode: Some("dontAsk".into()),
            current_turn_id: Some("turn-child".into()),
            ..Default::default()
        });
        child.usage = Some(Usage {
            input_tokens: Some(99),
            ..Default::default()
        });
        child.received_at = at(50);
        for kind in [EventKind::NewTurn, EventKind::Working, EventKind::Completed] {
            child.kind = kind;
            state.apply(&child, Some(&context("Child")));
        }
        assert!(state.reason.is_none());
        let mut after = state.snapshot.clone();
        assert_eq!(after.children.len(), 1);
        after.children.clear();
        after.updated_at = prior.updated_at;
        assert_eq!(after, prior);
    }

    #[test]
    fn unbounded_child_identity_is_rejected() {
        let prior = snapshot();
        let prior = Prior {
            snapshot: &prior,
            reason: None,
        };
        let long_id = "x".repeat(CHILD_AGENT_ID_MAX_CHARS + 1);
        let long_type = "x".repeat(CHILD_AGENT_TYPE_MAX_CHARS + 1);
        for (id, kind) in [
            ("", "Explore"),
            ("a", ""),
            ("a\nb", "Explore"),
            (long_id.as_str(), "Explore"),
            ("a", long_type.as_str()),
        ] {
            let mut value = event(EventKind::Working);
            value.child_agent = Some(ChildAgentRef {
                agent_id: id.into(),
                agent_type: kind.into(),
            });
            assert!(apply_event(prior, &value, None).is_err());
        }
    }

    /// Deterministic event corpus: every kind across every channel, with and
    /// without a reason, session, turn, tool, and source sequence, in a fixed
    /// pseudo-random order.
    fn corpus() -> Vec<Vec<(NormalizedEvent, Option<StatusReasonContext>)>> {
        let kinds = [
            EventKind::NewTurn,
            EventKind::Working,
            EventKind::Idle,
            EventKind::WaitingInput,
            EventKind::WaitingApproval,
            EventKind::Completed,
            EventKind::Failed,
            EventKind::Interrupted,
            EventKind::ProviderSessionStarted,
            EventKind::ProviderSessionEnded,
            EventKind::SessionEnded,
            EventKind::Enrichment,
        ];
        let channels = [
            EventEvidence::managed_hook(1),
            EventEvidence::local(EvidenceChannel::SideChannel),
            EventEvidence::local(EvidenceChannel::ProcessObservation),
            EventEvidence::local(EvidenceChannel::ProviderArtifact),
        ];
        let mut seed: u64 = 0x5eed;
        let mut next = move |bound: usize| {
            seed = seed
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            usize::try_from(seed >> 33).unwrap() % bound
        };
        (0..64)
            .map(|_| {
                (0_i64..24)
                    .map(|step| {
                        let mut value = event(kinds[next(kinds.len())].clone());
                        value.received_at = at(step * 600);
                        value.evidence = channels[next(channels.len())].clone();
                        if next(3) == 0 {
                            value.evidence.source_sequence = Some(u64::try_from(next(6)).unwrap());
                        }
                        if next(3) == 0 {
                            value.provider_session_id = Some(["a", "b"][next(2)].into());
                        }
                        if next(4) == 0 {
                            value.turn_id = Some(["t1", "t2"][next(2)].into());
                        }
                        if next(3) == 0 {
                            value.tool_activity = Some(ToolActivityUpdate {
                                phase: [
                                    ToolActivityPhase::Start,
                                    ToolActivityPhase::Progress,
                                    ToolActivityPhase::Finish,
                                ][next(3)],
                                label: "shell".into(),
                                correlation_id: None,
                                detail: None,
                            });
                        }
                        if next(4) == 0 {
                            value.usage = Some(Usage {
                                input_tokens: Some(u64::try_from(next(3)).unwrap()),
                                ..Default::default()
                            });
                        }
                        let summary = (next(2) == 0).then(|| context(["One", "Two"][next(2)]));
                        (value, summary)
                    })
                    .collect()
            })
            .collect()
    }

    #[test]
    fn transitions_are_deterministic_and_changed_iff_the_view_differs() {
        let mut observed_changes = 0;
        let mut observed_unchanged = 0;
        for sequence in corpus() {
            let mut state = State::new();
            for (step, (event, summary)) in sequence.iter().enumerate() {
                let first = apply_event(state.prior(), event, summary.as_ref()).unwrap();
                let second = apply_event(state.prior(), event, summary.as_ref()).unwrap();
                assert_eq!(first, second);
                let prior_view = project_public(&state.snapshot, state.reason.as_ref());
                let reason = first.reason.resolve(state.reason.as_ref()).cloned();
                let unbumped = project_public(&first.snapshot, reason.as_ref());
                let mut snapshot = first.snapshot.clone();
                let now = at(1_000_000 + i64::try_from(step).unwrap());
                let (view, changed) = finalize(&prior_view, &mut snapshot, reason.as_ref(), now);
                assert_eq!(changed.is_empty(), prior_view == unbumped);
                assert_eq!(view, project_public(&snapshot, reason.as_ref()));
                assert_eq!(
                    changed.is_empty(),
                    snapshot.updated_at == state.snapshot.updated_at
                );
                if changed.is_empty() {
                    observed_unchanged += 1;
                } else {
                    observed_changes += 1;
                }
                state.snapshot = snapshot;
                state.reason = reason;
            }
        }
        assert!(observed_changes > 100 && observed_unchanged > 100);
    }
}
