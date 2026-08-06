use axum::{
    Extension, Json,
    extract::{Path, Query, State},
};
use chrono::Utc;
use serde::{Deserialize, Serialize};

use crate::{
    ApiState, audit,
    auth::{AuthPrincipal, issue_token},
    error::ApiError,
    state::random_identifier,
};

#[derive(Serialize)]
pub(crate) struct WorkspaceResponse {
    id: String,
    name: String,
    slug: String,
    role: String,
}

#[derive(Deserialize)]
pub(crate) struct CreateWorkspaceRequest {
    name: String,
}

#[derive(Serialize)]
pub(crate) struct SwitchedWorkspaceResponse {
    token: String,
    workspace: WorkspaceResponse,
}

#[derive(Serialize)]
pub(crate) struct MemberResponse {
    user_id: String,
    email: String,
    role: String,
}

#[derive(Deserialize)]
pub(crate) struct AuditLogQuery {
    #[serde(default = "default_audit_limit")]
    limit: u64,
}

const fn default_audit_limit() -> u64 {
    200
}

#[derive(Serialize)]
pub(crate) struct AuditEventResponse {
    id: String,
    actor_type: String,
    actor_label: String,
    action: String,
    target_type: Option<String>,
    target_id: Option<String>,
    detail_json: Option<String>,
    created_at: String,
}

/// Lists every workspace the caller belongs to, for the sidebar switcher.
pub(crate) async fn list(
    Extension(principal): Extension<AuthPrincipal>,
    State(state): State<ApiState>,
) -> Result<Json<Vec<WorkspaceResponse>>, ApiError> {
    let workspaces = state
        .metadata()?
        .workspaces_for_user(&principal.subject)
        .map_err(ApiError::internal)?
        .into_iter()
        .map(|(workspace, role)| WorkspaceResponse {
            id: workspace.id,
            name: workspace.name,
            slug: workspace.slug,
            role,
        })
        .collect();
    Ok(Json(workspaces))
}

/// Creates a new workspace and switches the caller into it immediately.
pub(crate) async fn create(
    Extension(principal): Extension<AuthPrincipal>,
    State(state): State<ApiState>,
    Json(request): Json<CreateWorkspaceRequest>,
) -> Result<Json<SwitchedWorkspaceResponse>, ApiError> {
    let name = request.name.trim();
    if name.is_empty() || name.chars().count() > 80 {
        return Err(ApiError::bad_request(
            "workspace name must be 1-80 characters",
        ));
    }
    let workspace_id = random_identifier("ws_", 16);
    let slug = workspace_id.trim_start_matches("ws_").to_owned();
    let now = Utc::now().to_rfc3339();
    let metadata = state.metadata()?;
    metadata
        .create_workspace(&workspace_id, name, &slug, &now)
        .map_err(ApiError::internal)?;
    metadata
        .add_workspace_member(&workspace_id, &principal.subject, "admin", &now)
        .map_err(ApiError::internal)?;
    let token = issue_token(&state, &principal.subject, "admin", &workspace_id)?;
    audit::record_in(
        &state,
        &workspace_id,
        &principal,
        "workspace.create",
        Some(("workspace", &workspace_id)),
        Some(serde_json::json!({"name": name})),
    );
    Ok(Json(SwitchedWorkspaceResponse {
        token,
        workspace: WorkspaceResponse {
            id: workspace_id,
            name: name.to_owned(),
            slug,
            role: "admin".to_owned(),
        },
    }))
}

/// Switches the caller's active session into another workspace they belong
/// to, minting a token scoped to it.
pub(crate) async fn switch(
    Extension(principal): Extension<AuthPrincipal>,
    State(state): State<ApiState>,
    Path(workspace_id): Path<String>,
) -> Result<Json<SwitchedWorkspaceResponse>, ApiError> {
    let metadata = state.metadata()?;
    let workspace = metadata
        .workspace_by_id(&workspace_id)
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::not_found("workspace does not exist"))?;
    let role = metadata
        .workspace_member_role(&workspace_id, &principal.subject)
        .map_err(ApiError::internal)?
        .ok_or_else(|| ApiError::forbidden("you are not a member of this workspace"))?;
    let token = issue_token(&state, &principal.subject, &role, &workspace_id)?;
    Ok(Json(SwitchedWorkspaceResponse {
        token,
        workspace: WorkspaceResponse {
            id: workspace.id,
            name: workspace.name,
            slug: workspace.slug,
            role,
        },
    }))
}

/// Lists the members of the caller's current workspace.
pub(crate) async fn members(
    Extension(principal): Extension<AuthPrincipal>,
    State(state): State<ApiState>,
) -> Result<Json<Vec<MemberResponse>>, ApiError> {
    let workspace_id = principal.require_workspace()?;
    let members = state
        .metadata()?
        .list_workspace_members(workspace_id)
        .map_err(ApiError::internal)?
        .into_iter()
        .map(|member| MemberResponse {
            user_id: member.user_id,
            email: member.email,
            role: member.role,
        })
        .collect();
    Ok(Json(members))
}

/// Lists the audit trail for the caller's current workspace, most recent
/// first. Admin only: entries can include query SQL text and other
/// sensitive detail.
pub(crate) async fn audit_log(
    Extension(principal): Extension<AuthPrincipal>,
    State(state): State<ApiState>,
    Query(query): Query<AuditLogQuery>,
) -> Result<Json<Vec<AuditEventResponse>>, ApiError> {
    principal.require_admin()?;
    let workspace_id = principal.require_workspace()?;
    let limit = query.limit.clamp(1, 1_000);
    let events = state
        .metadata()?
        .audit_log_in_workspace(workspace_id, limit)
        .map_err(ApiError::internal)?
        .into_iter()
        .map(|event| AuditEventResponse {
            id: event.id,
            actor_type: event.actor_type,
            actor_label: event.actor_label,
            action: event.action,
            target_type: event.target_type,
            target_id: event.target_id,
            detail_json: event.detail_json,
            created_at: event.created_at,
        })
        .collect();
    Ok(Json(events))
}

/// Removes a member from the caller's current workspace. Admin only; the
/// caller cannot remove themselves this way.
pub(crate) async fn remove_member(
    Extension(principal): Extension<AuthPrincipal>,
    State(state): State<ApiState>,
    Path(user_id): Path<String>,
) -> Result<axum::http::StatusCode, ApiError> {
    principal.require_admin()?;
    let workspace_id = principal.require_workspace()?;
    if user_id == principal.subject {
        return Err(ApiError::bad_request(
            "you cannot remove yourself from the workspace",
        ));
    }
    let removed = state
        .metadata()?
        .remove_workspace_member(workspace_id, &user_id)
        .map_err(ApiError::internal)?;
    if removed {
        audit::record(
            &state,
            &principal,
            "workspace.remove_member",
            Some(("user", &user_id)),
            None,
        );
        Ok(axum::http::StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::not_found("member does not exist"))
    }
}
