//! Durable audit trail: every mutating action taken by every dashboard user,
//! scoped to the workspace their session is in. API-key sessions (headless
//! automation against a single database, not a signed-in person) are not
//! logged here — that is a separate concern from "what did each user do."
//!
//! Logging failures do not fail the request that triggered them — the
//! primary action already succeeded (or is about to be reported to the
//! caller) by the time this runs, and losing the audit trail for one event
//! is preferable to losing the underlying work.

use chrono::Utc;
use serde_json::Value;

use crate::{ApiState, auth::AuthPrincipal, state::random_identifier};

/// Records one audit event in the caller's current workspace. `target` is
/// `(type, id)`, e.g. `("database", "db_abc123")`. A no-op for API-key
/// sessions, which have no workspace to scope into. Errors are logged to
/// stderr rather than propagated, per the module-level rationale above.
pub(crate) fn record(
    state: &ApiState,
    principal: &AuthPrincipal,
    action: &str,
    target: Option<(&str, &str)>,
    detail: Option<Value>,
) {
    let Some(workspace_id) = principal.workspace_id.clone() else {
        return;
    };
    record_in(state, &workspace_id, principal, action, target, detail);
}

/// Records one audit event in an explicit workspace, for the rare action
/// (creating a workspace, accepting an invite into one) that targets a
/// workspace other than the one the caller's session is currently scoped
/// to.
pub(crate) fn record_in(
    state: &ApiState,
    workspace_id: &str,
    principal: &AuthPrincipal,
    action: &str,
    target: Option<(&str, &str)>,
    detail: Option<Value>,
) {
    if let Err(error) = try_record(state, workspace_id, principal, action, target, detail) {
        eprintln!("audit log: failed to record '{action}' in {workspace_id}: {error}");
    }
}

fn try_record(
    state: &ApiState,
    workspace_id: &str,
    principal: &AuthPrincipal,
    action: &str,
    target: Option<(&str, &str)>,
    detail: Option<Value>,
) -> anyhow::Result<()> {
    let metadata = state
        .metadata()
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    let (actor_type, actor_label) = if principal.database_id.is_some() {
        ("api_key", principal.subject.clone())
    } else {
        let label = metadata
            .user_by_id(&principal.subject)?
            .map_or_else(|| principal.subject.clone(), |user| user.email);
        ("user", label)
    };
    let detail_json = detail.map(|value| value.to_string());
    metadata.record_audit_event(&pintail_meta::NewAuditEvent {
        id: &random_identifier("audit_", 16),
        workspace_id,
        actor_type,
        actor_id: &principal.subject,
        actor_label: &actor_label,
        action,
        target_type: target.map(|(kind, _)| kind),
        target_id: target.map(|(_, id)| id),
        detail_json: detail_json.as_deref(),
        created_at: &Utc::now().to_rfc3339(),
        client_ip: principal.client_ip.as_deref(),
    })
}
