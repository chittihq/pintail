//! Workspace invites. An admin creates one for a specific email and role;
//! the raw token is shown once (copy-link only — Pintail never sends mail)
//! and is redeemed by signing in with Google using that exact email
//! ([`crate::oauth`]).

use axum::{
    Extension, Json,
    extract::{Path, Query, State},
    http::StatusCode,
};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::{
    ApiState, audit, auth::AuthPrincipal, error::ApiError, oauth::invite_lifetime_days,
    state::random_identifier,
};

#[derive(Serialize)]
pub(crate) struct InviteResponse {
    id: String,
    email: String,
    role: String,
    created_at: String,
    expires_at: String,
    accepted_at: Option<String>,
    revoked_at: Option<String>,
}

#[derive(Serialize)]
pub(crate) struct CreatedInviteResponse {
    #[serde(flatten)]
    invite: InviteResponse,
    /// The raw, unhashed invite link token. Returned only this once.
    token: String,
}

#[derive(Deserialize)]
pub(crate) struct CreateInviteRequest {
    email: String,
    role: String,
}

#[derive(Deserialize)]
pub(crate) struct InviteStatusQuery {
    token: String,
}

#[derive(Serialize)]
pub(crate) struct InviteStatusResponse {
    valid: bool,
    email: Option<String>,
    role: Option<String>,
    workspace_name: Option<String>,
    reason: Option<&'static str>,
}

/// Public lookup for the invite-acceptance page: shows which workspace,
/// email, and role a link grants before the visitor signs in with Google.
/// Reveals no more than what the admin already put in the (email-bound)
/// link itself.
pub(crate) async fn status(
    State(state): State<ApiState>,
    Query(query): Query<InviteStatusQuery>,
) -> Result<Json<InviteStatusResponse>, ApiError> {
    let metadata = state.metadata()?;
    let token_hash = Sha256::digest(query.token.as_bytes());
    let Some(invite) = metadata
        .invite_by_token_hash(&token_hash)
        .map_err(ApiError::internal)?
    else {
        return Ok(Json(InviteStatusResponse {
            valid: false,
            email: None,
            role: None,
            workspace_name: None,
            reason: Some("not_found"),
        }));
    };
    let reason = if invite.revoked_at.is_some() {
        Some("revoked")
    } else if invite.accepted_at.is_some() {
        Some("accepted")
    } else if DateTime::parse_from_rfc3339(&invite.expires_at)
        .is_ok_and(|expires| expires <= Utc::now())
    {
        Some("expired")
    } else {
        None
    };
    let workspace_name = metadata
        .workspace_by_id(&invite.workspace_id)
        .map_err(ApiError::internal)?
        .map(|workspace| workspace.name);
    Ok(Json(InviteStatusResponse {
        valid: reason.is_none(),
        email: Some(invite.email),
        role: Some(invite.role),
        workspace_name,
        reason,
    }))
}

/// Lists every invite issued in the caller's current workspace, most recent
/// first, so admins can see pending / accepted / revoked state.
pub(crate) async fn list(
    Extension(principal): Extension<AuthPrincipal>,
    State(state): State<ApiState>,
) -> Result<Json<Vec<InviteResponse>>, ApiError> {
    principal.require_admin()?;
    let workspace_id = principal.require_workspace()?;
    let invites = state
        .metadata()?
        .invites_in_workspace(workspace_id)
        .map_err(ApiError::internal)?
        .into_iter()
        .map(|invite| InviteResponse {
            id: invite.id,
            email: invite.email,
            role: invite.role,
            created_at: invite.created_at,
            expires_at: invite.expires_at,
            accepted_at: invite.accepted_at,
            revoked_at: invite.revoked_at,
        })
        .collect();
    Ok(Json(invites))
}

/// Creates a pending invite for one email into the caller's current
/// workspace, with the role the admin chose for it.
pub(crate) async fn create(
    Extension(principal): Extension<AuthPrincipal>,
    State(state): State<ApiState>,
    Json(request): Json<CreateInviteRequest>,
) -> Result<Json<CreatedInviteResponse>, ApiError> {
    principal.require_admin()?;
    let workspace_id = principal.require_workspace()?;
    let email = request.email.trim().to_ascii_lowercase();
    if email.is_empty() || !email.contains('@') {
        return Err(ApiError::bad_request("a valid email address is required"));
    }
    if !matches!(request.role.as_str(), "admin" | "operator" | "viewer") {
        return Err(ApiError::bad_request(
            "role must be admin, operator, or viewer",
        ));
    }

    let metadata = state.metadata()?;
    if let Some(user) = metadata.user_by_email(&email).map_err(ApiError::internal)? {
        let already_member = metadata
            .workspace_member_role(workspace_id, &user.id)
            .map_err(ApiError::internal)?
            .is_some();
        if already_member {
            return Err(ApiError::conflict(
                "this email already belongs to the workspace",
            ));
        }
    }

    let id = random_identifier("inv_", 16);
    let raw_token = random_identifier("invtok_", 32);
    let token_hash = Sha256::digest(raw_token.as_bytes()).to_vec();
    let now = Utc::now();
    let created_at = now.to_rfc3339();
    let expires_at = (now + Duration::days(invite_lifetime_days())).to_rfc3339();

    metadata
        .create_invite(&pintail_meta::NewInvite {
            id: &id,
            token_hash: &token_hash,
            workspace_id,
            email: &email,
            role: &request.role,
            created_by: &principal.subject,
            created_at: &created_at,
            expires_at: &expires_at,
        })
        .map_err(ApiError::internal)?;

    audit::record(
        &state,
        &principal,
        "invite.create",
        Some(("invite", &id)),
        Some(serde_json::json!({"email": email, "role": request.role})),
    );

    Ok(Json(CreatedInviteResponse {
        invite: InviteResponse {
            id,
            email,
            role: request.role,
            created_at,
            expires_at,
            accepted_at: None,
            revoked_at: None,
        },
        token: raw_token,
    }))
}

/// Revokes a pending invite in the caller's current workspace so its link
/// can no longer be redeemed.
pub(crate) async fn revoke(
    Extension(principal): Extension<AuthPrincipal>,
    State(state): State<ApiState>,
    Path(invite_id): Path<String>,
) -> Result<StatusCode, ApiError> {
    principal.require_admin()?;
    let workspace_id = principal.require_workspace()?;
    let metadata = state.metadata()?;
    let invite = metadata
        .invites_in_workspace(workspace_id)
        .map_err(ApiError::internal)?
        .into_iter()
        .find(|invite| invite.id == invite_id)
        .ok_or_else(|| ApiError::not_found("invite does not exist"))?;
    let revoked = metadata
        .revoke_invite(&invite.id, &Utc::now().to_rfc3339())
        .map_err(ApiError::internal)?;
    if revoked {
        audit::record(
            &state,
            &principal,
            "invite.revoke",
            Some(("invite", &invite_id)),
            None,
        );
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::conflict("invite was already accepted"))
    }
}
