use anyhow::{Context, Result, bail};
use rusqlite::{OptionalExtension as _, params};

use crate::MetaStore;

/// Durable dashboard user.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UserRecord {
    pub id: String,
    pub email: String,
    pub argon2_hash: String,
    pub role: String,
    pub enabled: bool,
    pub created_at: String,
    pub last_login_at: Option<String>,
    /// Google OIDC subject, once this identity has signed in with Google.
    pub google_subject: Option<String>,
}

/// A pending, accepted, or revoked invitation into a workspace.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InviteRecord {
    pub id: String,
    pub workspace_id: String,
    pub email: String,
    pub role: String,
    pub created_by: String,
    pub created_at: String,
    pub expires_at: String,
    pub accepted_at: Option<String>,
    pub revoked_at: Option<String>,
}

/// Values for one newly generated invite. The raw token is hashed for
/// storage and only ever returned once, at creation, matching how API-key
/// secrets are handled.
/// One invited Google identity being admitted to a workspace.
///
/// Grouped because the three writes it drives must succeed or fail together;
/// passing them as separate arguments invited the split that made a partial
/// signup possible.
pub struct GoogleAdmission<'a> {
    pub user_id: &'a str,
    pub email: &'a str,
    pub google_subject: &'a str,
    pub workspace_id: &'a str,
    pub invite_id: &'a str,
    pub role: &'a str,
    pub now: &'a str,
}

pub struct NewInvite<'a> {
    pub id: &'a str,
    pub token_hash: &'a [u8],
    pub workspace_id: &'a str,
    pub email: &'a str,
    pub role: &'a str,
    pub created_by: &'a str,
    pub created_at: &'a str,
    pub expires_at: &'a str,
}

/// One durable audit-log entry: who did what to what, within a workspace.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuditEventRecord {
    pub id: String,
    pub workspace_id: String,
    pub actor_type: String,
    pub actor_id: String,
    pub actor_label: String,
    pub action: String,
    pub target_type: Option<String>,
    pub target_id: Option<String>,
    pub detail_json: Option<String>,
    pub created_at: String,
}

/// Values for one newly recorded audit event.
pub struct NewAuditEvent<'a> {
    pub id: &'a str,
    pub workspace_id: &'a str,
    pub actor_type: &'a str,
    pub actor_id: &'a str,
    pub actor_label: &'a str,
    pub action: &'a str,
    pub target_type: Option<&'a str>,
    pub target_id: Option<&'a str>,
    pub detail_json: Option<&'a str>,
    pub created_at: &'a str,
}

/// Durable source-database configuration and status.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DatabaseRecord {
    pub id: String,
    pub name: String,
    pub encrypted_dsn: Vec<u8>,
    pub mode: String,
    pub effective_mode: Option<String>,
    pub state: String,
    pub probe_json: Option<String>,
    pub include_tables: Option<String>,
    pub exclude_tables: Option<String>,
    pub poll_interval_seconds: u64,
    pub reconcile_interval_seconds: u64,
    pub created_at: String,
    pub updated_at: String,
    /// Keyless-table replication policy: `quarantine`, `auto_resync`, or
    /// `reject`.
    pub keyless_policy: String,
    /// `replicated` (MySQL-mirrored, the default) or `local` (writable,
    /// docs/design/writable-mode.md).
    pub kind: String,
    /// Owning workspace. Absent only for rows created before workspaces
    /// existed on a node that has not yet been migrated to schema 15.
    pub workspace_id: Option<String>,
}

/// Durable workspace: a named boundary for databases and team membership.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceRecord {
    pub id: String,
    pub name: String,
    pub slug: String,
    pub created_at: String,
}

/// One user's membership and role within a workspace.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceMemberRecord {
    pub workspace_id: String,
    pub user_id: String,
    pub email: String,
    pub role: String,
    pub created_at: String,
}

/// Mutable source-database settings.
pub struct DatabaseUpdate<'a> {
    pub name: &'a str,
    pub encrypted_dsn: Option<&'a [u8]>,
    pub mode: &'a str,
    pub include_tables: Option<&'a str>,
    pub exclude_tables: Option<&'a str>,
    pub poll_interval_seconds: u64,
    pub reconcile_interval_seconds: u64,
    pub keyless_policy: &'a str,
    pub now: &'a str,
}

/// Durable source-table status.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TableRecord {
    pub database_id: String,
    pub name: String,
    pub state: String,
    pub primary_key_json: Option<String>,
    pub cursor_column: Option<String>,
    pub sort_key_json: Option<String>,
    pub rows_synced: u64,
    pub last_error: Option<String>,
    pub last_reconcile_at: Option<String>,
    pub schema_version: u32,
    pub orphaned_at: Option<String>,
    pub soft_delete_column: Option<String>,
}

/// Durable database-scoped API key metadata.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ApiKeyRecord {
    pub id: String,
    pub database_id: String,
    pub name: String,
    pub sha256: Vec<u8>,
    pub mysql_native_password_hash: Option<Vec<u8>>,
    pub caching_sha2_password_hash: Option<Vec<u8>>,
    pub enabled: bool,
    pub scopes_json: String,
    pub expires_at: Option<String>,
    pub last_used_at: Option<String>,
    pub created_at: String,
}

/// Values for one newly generated API key.
pub struct NewApiKey<'a> {
    pub id: &'a str,
    pub database_id: &'a str,
    pub name: &'a str,
    pub sha256: &'a [u8],
    pub mysql_native_password_hash: Option<&'a [u8]>,
    pub caching_sha2_password_hash: Option<&'a [u8]>,
    pub scopes_json: &'a str,
    pub expires_at: Option<&'a str>,
    pub now: &'a str,
}

/// One sync activity record.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SyncRunRecord {
    pub id: String,
    pub database_id: String,
    pub table_name: Option<String>,
    pub kind: String,
    pub status: String,
    pub rows: u64,
    pub bytes: u64,
    pub duration_ms: Option<u64>,
    pub error: Option<String>,
    pub started_at: String,
}

/// One dead-letter record.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DlqRecord {
    pub id: String,
    pub database_id: String,
    pub table_name: Option<String>,
    pub event_json: String,
    pub error: String,
    pub created_at: String,
}

impl MetaStore {
    /// Returns the number of configured users.
    ///
    /// # Errors
    ///
    /// Returns an error when the user table cannot be counted.
    pub fn user_count(&self) -> Result<u64> {
        let count: i64 = self
            .connection
            .query_row("SELECT COUNT(*) FROM users", [], |row| row.get(0))
            .context("failed to count users")?;
        u64::try_from(count).context("user count is negative")
    }

    /// Creates one dashboard user.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid role, duplicate identity, or storage
    /// failure.
    pub fn create_user(
        &self,
        id: &str,
        email: &str,
        argon2_hash: &str,
        role: &str,
        now: &str,
    ) -> Result<()> {
        validate_role(role)?;
        self.connection
            .execute(
                "INSERT INTO users (id, email, argon2_hash, role, created_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                (id, email, argon2_hash, role, now),
            )
            .context("failed to create user")?;
        Ok(())
    }

    /// Returns a user by case-insensitive email.
    ///
    /// # Errors
    ///
    /// Returns an error when the user record cannot be read or decoded.
    pub fn user_by_email(&self, email: &str) -> Result<Option<UserRecord>> {
        self.connection
            .query_row(
                "SELECT id, email, argon2_hash, role, enabled, created_at, last_login_at, \
                        google_subject \
                 FROM users WHERE email = ?1 COLLATE NOCASE",
                [email],
                decode_user,
            )
            .optional()
            .context("failed to read user")
    }

    /// Returns a user by id.
    ///
    /// # Errors
    ///
    /// Returns an error when the user record cannot be read or decoded.
    pub fn user_by_id(&self, id: &str) -> Result<Option<UserRecord>> {
        self.connection
            .query_row(
                "SELECT id, email, argon2_hash, role, enabled, created_at, last_login_at, \
                        google_subject \
                 FROM users WHERE id = ?1",
                [id],
                decode_user,
            )
            .optional()
            .context("failed to read user")
    }

    /// Returns a user by their Google OIDC subject.
    ///
    /// # Errors
    ///
    /// Returns an error when the user record cannot be read or decoded.
    pub fn user_by_google_subject(&self, subject: &str) -> Result<Option<UserRecord>> {
        self.connection
            .query_row(
                "SELECT id, email, argon2_hash, role, enabled, created_at, last_login_at, \
                        google_subject \
                 FROM users WHERE google_subject = ?1",
                [subject],
                decode_user,
            )
            .optional()
            .context("failed to read user")
    }

    /// Links a Google OIDC subject to an existing user without replacing a
    /// different subject already bound to that account.
    ///
    /// # Errors
    ///
    /// Returns an error when the user is absent, either identity is already
    /// linked differently, or the row cannot be updated.
    pub fn set_user_google_subject(&self, user_id: &str, subject: &str) -> Result<()> {
        let changed = self
            .connection
            .execute(
                "UPDATE users SET google_subject = ?2 \
                 WHERE id = ?1 AND (google_subject IS NULL OR google_subject = ?2)",
                (user_id, subject),
            )
            .context("failed to link Google identity")?;
        if changed == 1 {
            Ok(())
        } else {
            bail!("user is absent or already linked to another Google identity")
        }
    }

    /// Lists users in email order.
    ///
    /// # Errors
    ///
    /// Returns an error when user records cannot be read or decoded.
    pub fn users(&self) -> Result<Vec<UserRecord>> {
        let mut statement = self
            .connection
            .prepare(
                "SELECT id, email, argon2_hash, role, enabled, created_at, last_login_at, \
                        google_subject \
                 FROM users ORDER BY email COLLATE NOCASE",
            )
            .context("failed to prepare user query")?;
        statement
            .query_map([], decode_user)
            .context("failed to query users")?
            .collect::<rusqlite::Result<_>>()
            .context("failed to decode users")
    }

    /// Records a successful login.
    ///
    /// # Errors
    ///
    /// Returns an error when the user record cannot be updated.
    pub fn touch_user_login(&self, id: &str, now: &str) -> Result<()> {
        self.connection
            .execute(
                "UPDATE users SET last_login_at = ?2 WHERE id = ?1",
                (id, now),
            )
            .context("failed to update user login")?;
        Ok(())
    }

    /// Lists configured source databases.
    ///
    /// # Errors
    ///
    /// Returns an error when database records cannot be read or decoded.
    pub fn databases(&self) -> Result<Vec<DatabaseRecord>> {
        let mut statement = self
            .connection
            .prepare(&format!(
                "{} ORDER BY name COLLATE NOCASE",
                database_select_sql()
            ))
            .context("failed to prepare database query")?;
        statement
            .query_map([], decode_database)
            .context("failed to query databases")?
            .collect::<rusqlite::Result<Vec<_>>>()
            .context("failed to decode databases")
    }

    /// Returns one configured source database.
    ///
    /// # Errors
    ///
    /// Returns an error when the database record cannot be read or decoded.
    pub fn database(&self, id: &str) -> Result<Option<DatabaseRecord>> {
        self.connection
            .query_row(
                &format!("{} WHERE id = ?1", database_select_sql()),
                [id],
                decode_database,
            )
            .optional()
            .context("failed to read database")
    }

    /// Lists the source databases that belong to one workspace.
    ///
    /// # Errors
    ///
    /// Returns an error when database records cannot be read or decoded.
    pub fn databases_in_workspace(&self, workspace_id: &str) -> Result<Vec<DatabaseRecord>> {
        let mut statement = self
            .connection
            .prepare(&format!(
                "{} WHERE workspace_id = ?1 ORDER BY name COLLATE NOCASE",
                database_select_sql()
            ))
            .context("failed to prepare database query")?;
        statement
            .query_map([workspace_id], decode_database)
            .context("failed to query databases")?
            .collect::<rusqlite::Result<Vec<_>>>()
            .context("failed to decode databases")
    }

    /// Returns one database, only when it belongs to the given workspace.
    ///
    /// # Errors
    ///
    /// Returns an error when the database record cannot be read or decoded.
    pub fn database_in_workspace(
        &self,
        id: &str,
        workspace_id: &str,
    ) -> Result<Option<DatabaseRecord>> {
        self.connection
            .query_row(
                &format!(
                    "{} WHERE id = ?1 AND workspace_id = ?2",
                    database_select_sql()
                ),
                [id, workspace_id],
                decode_database,
            )
            .optional()
            .context("failed to read database")
    }

    /// Creates a workspace.
    ///
    /// # Errors
    ///
    /// Returns an error when the id or slug is already taken, or the row
    /// cannot be written.
    pub fn create_workspace(&self, id: &str, name: &str, slug: &str, now: &str) -> Result<()> {
        self.connection
            .execute(
                "INSERT INTO workspaces (id, name, slug, created_at) VALUES (?1, ?2, ?3, ?4)",
                (id, name, slug, now),
            )
            .context("failed to create workspace")?;
        Ok(())
    }

    /// Returns one workspace by id.
    ///
    /// # Errors
    ///
    /// Returns an error when the workspace record cannot be read or decoded.
    pub fn workspace_by_id(&self, id: &str) -> Result<Option<WorkspaceRecord>> {
        self.connection
            .query_row(
                "SELECT id, name, slug, created_at FROM workspaces WHERE id = ?1",
                [id],
                decode_workspace,
            )
            .optional()
            .context("failed to read workspace")
    }

    /// Returns one workspace by its unique slug.
    ///
    /// # Errors
    ///
    /// Returns an error when the workspace record cannot be read or decoded.
    pub fn workspace_by_slug(&self, slug: &str) -> Result<Option<WorkspaceRecord>> {
        self.connection
            .query_row(
                "SELECT id, name, slug, created_at FROM workspaces WHERE slug = ?1",
                [slug],
                decode_workspace,
            )
            .optional()
            .context("failed to read workspace")
    }

    /// Lists every workspace a user belongs to, alongside their role in each.
    ///
    /// # Errors
    ///
    /// Returns an error when workspace records cannot be read or decoded.
    pub fn workspaces_for_user(&self, user_id: &str) -> Result<Vec<(WorkspaceRecord, String)>> {
        let mut statement = self
            .connection
            .prepare(
                "SELECT w.id, w.name, w.slug, w.created_at, m.role \
                 FROM workspaces w \
                 JOIN workspace_members m ON m.workspace_id = w.id \
                 WHERE m.user_id = ?1 \
                 ORDER BY w.name COLLATE NOCASE",
            )
            .context("failed to prepare workspace query")?;
        statement
            .query_map([user_id], |row| {
                Ok((
                    WorkspaceRecord {
                        id: row.get(0)?,
                        name: row.get(1)?,
                        slug: row.get(2)?,
                        created_at: row.get(3)?,
                    },
                    row.get(4)?,
                ))
            })
            .context("failed to query workspaces")?
            .collect::<rusqlite::Result<Vec<_>>>()
            .context("failed to decode workspaces")
    }

    /// Adds one member to a workspace with the given role.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid role or a storage failure.
    pub fn add_workspace_member(
        &self,
        workspace_id: &str,
        user_id: &str,
        role: &str,
        now: &str,
    ) -> Result<()> {
        validate_role(role)?;
        self.connection
            .execute(
                "INSERT INTO workspace_members (workspace_id, user_id, role, created_at) \
                 VALUES (?1, ?2, ?3, ?4) \
                 ON CONFLICT(workspace_id, user_id) DO UPDATE SET role = excluded.role",
                (workspace_id, user_id, role, now),
            )
            .context("failed to add workspace member")?;
        Ok(())
    }

    /// Returns one user's role in a workspace, when they are a member.
    ///
    /// # Errors
    ///
    /// Returns an error when the membership record cannot be read.
    pub fn workspace_member_role(
        &self,
        workspace_id: &str,
        user_id: &str,
    ) -> Result<Option<String>> {
        self.connection
            .query_row(
                "SELECT role FROM workspace_members WHERE workspace_id = ?1 AND user_id = ?2",
                [workspace_id, user_id],
                |row| row.get(0),
            )
            .optional()
            .context("failed to read workspace membership")
    }

    /// Lists the members of one workspace with their email and role.
    ///
    /// # Errors
    ///
    /// Returns an error when membership records cannot be read or decoded.
    pub fn list_workspace_members(&self, workspace_id: &str) -> Result<Vec<WorkspaceMemberRecord>> {
        let mut statement = self
            .connection
            .prepare(
                "SELECT m.workspace_id, m.user_id, u.email, m.role, m.created_at \
                 FROM workspace_members m \
                 JOIN users u ON u.id = m.user_id \
                 WHERE m.workspace_id = ?1 \
                 ORDER BY u.email COLLATE NOCASE",
            )
            .context("failed to prepare workspace member query")?;
        statement
            .query_map([workspace_id], |row| {
                Ok(WorkspaceMemberRecord {
                    workspace_id: row.get(0)?,
                    user_id: row.get(1)?,
                    email: row.get(2)?,
                    role: row.get(3)?,
                    created_at: row.get(4)?,
                })
            })
            .context("failed to query workspace members")?
            .collect::<rusqlite::Result<Vec<_>>>()
            .context("failed to decode workspace members")
    }

    /// Removes one member from a workspace.
    ///
    /// # Errors
    ///
    /// Returns an error when the membership row cannot be deleted.
    pub fn remove_workspace_member(&self, workspace_id: &str, user_id: &str) -> Result<bool> {
        self.connection
            .execute(
                "DELETE FROM workspace_members WHERE workspace_id = ?1 AND user_id = ?2",
                [workspace_id, user_id],
            )
            .map(|changed| changed == 1)
            .context("failed to remove workspace member")
    }

    /// Creates a user with no password, for an identity that will only ever
    /// sign in with Google (invite acceptance).
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid role, duplicate identity, or storage
    /// failure.
    pub fn create_user_via_google(
        &self,
        id: &str,
        email: &str,
        google_subject: &str,
        role: &str,
        now: &str,
    ) -> Result<()> {
        validate_role(role)?;
        self.connection
            .execute(
                "INSERT INTO users (id, email, argon2_hash, role, created_at, google_subject) \
                 VALUES (?1, ?2, '', ?3, ?4, ?5)",
                (id, email, role, now, google_subject),
            )
            .context("failed to create user")?;
        Ok(())
    }

    /// Admits an invited Google identity: creates the user, grants the
    /// workspace membership, and consumes the invite as one unit.
    ///
    /// These were three separate writes. A failure or restart between the
    /// first and the second left an account that could never sign in again:
    /// the user row exists, so every later attempt skips the invite path, but
    /// no membership exists, so it is refused for belonging to no workspace.
    /// If the third had also run, the invite was spent as well - leaving no
    /// route back in through the UI at all.
    ///
    /// One transaction means a partial admission rolls back entirely and the
    /// invite stays usable, so the operator's remedy is simply to try again.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid role or a storage failure. The
    /// transaction is rolled back on any of them.
    pub fn admit_invited_google_user(&self, admission: &GoogleAdmission<'_>) -> Result<()> {
        validate_role(admission.role)?;
        let transaction = self
            .connection
            .unchecked_transaction()
            .context("failed to begin Google admission")?;
        transaction
            .execute(
                "INSERT INTO users (id, email, argon2_hash, role, created_at, google_subject) \
                 VALUES (?1, ?2, '', ?3, ?4, ?5)",
                (
                    admission.user_id,
                    admission.email,
                    admission.role,
                    admission.now,
                    admission.google_subject,
                ),
            )
            .context("failed to create user")?;
        transaction
            .execute(
                "INSERT INTO workspace_members (workspace_id, user_id, role, created_at) \
                 VALUES (?1, ?2, ?3, ?4) \
                 ON CONFLICT(workspace_id, user_id) DO UPDATE SET role = excluded.role",
                (
                    admission.workspace_id,
                    admission.user_id,
                    admission.role,
                    admission.now,
                ),
            )
            .context("failed to add workspace member")?;
        claim_invite(&transaction, admission)?;
        transaction
            .commit()
            .context("failed to commit Google admission")?;
        Ok(())
    }

    /// Admits an identity that already has a user row into the workspace its
    /// invite names, consuming that invite in the same transaction.
    ///
    /// The sign-in path resolves an existing Google identity by subject and
    /// returns immediately, so before this existed an invite could not reach
    /// anyone who already had an account. Two very different people land in
    /// that state: someone left with no membership at all by the pre-atomic
    /// admission, for whom every later sign-in was refused for belonging to no
    /// workspace and no fresh invite could help; and an ordinary member being
    /// invited into a second workspace, whose invite simply stayed pending
    /// forever while they signed into their existing one.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid role or a storage failure, including an
    /// invite that is no longer claimable. The transaction is rolled back on
    /// any of them.
    pub fn admit_existing_user_via_invite(&self, admission: &GoogleAdmission<'_>) -> Result<()> {
        validate_role(admission.role)?;
        let transaction = self
            .connection
            .unchecked_transaction()
            .context("failed to begin invite admission")?;
        transaction
            .execute(
                "INSERT INTO workspace_members (workspace_id, user_id, role, created_at) \
                 VALUES (?1, ?2, ?3, ?4) \
                 ON CONFLICT(workspace_id, user_id) DO UPDATE SET role = excluded.role",
                (
                    admission.workspace_id,
                    admission.user_id,
                    admission.role,
                    admission.now,
                ),
            )
            .context("failed to add workspace member")?;
        claim_invite(&transaction, admission)?;
        transaction
            .commit()
            .context("failed to commit invite admission")?;
        Ok(())
    }

    /// Reads one invite by id.
    ///
    /// # Errors
    ///
    /// Returns an error when the invite cannot be read or decoded.
    pub fn invite_by_id(&self, id: &str) -> Result<Option<InviteRecord>> {
        let mut statement = self
            .connection
            .prepare(&format!("{} WHERE id = ?1", invite_select_sql()))
            .context("failed to prepare invite query")?;
        statement
            .query_row([id], decode_invite)
            .optional()
            .context("failed to read invite")
    }

    /// Creates a pending invite. The raw token is never stored; only its
    /// hash is, matching how API-key secrets are handled.
    ///
    /// (see `claim_invite` below for the shared compare-and-set)
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid role or a storage failure.
    pub fn create_invite(&self, invite: &NewInvite<'_>) -> Result<()> {
        validate_role(invite.role)?;
        self.connection
            .execute(
                "INSERT INTO invites (\
                   id, token_hash, workspace_id, email, role, created_by, created_at, expires_at\
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    invite.id,
                    invite.token_hash,
                    invite.workspace_id,
                    invite.email,
                    invite.role,
                    invite.created_by,
                    invite.created_at,
                    invite.expires_at,
                ],
            )
            .context("failed to create invite")?;
        Ok(())
    }

    /// Finds an invite by its token hash.
    ///
    /// # Errors
    ///
    /// Returns an error when the invite record cannot be read or decoded.
    pub fn invite_by_token_hash(&self, token_hash: &[u8]) -> Result<Option<InviteRecord>> {
        self.connection
            .query_row(
                &format!("{} WHERE token_hash = ?1", invite_select_sql()),
                [token_hash],
                decode_invite,
            )
            .optional()
            .context("failed to read invite")
    }

    /// Lists invites created within one workspace, most recent first.
    ///
    /// # Errors
    ///
    /// Returns an error when invite records cannot be read or decoded.
    pub fn invites_in_workspace(&self, workspace_id: &str) -> Result<Vec<InviteRecord>> {
        let mut statement = self
            .connection
            .prepare(&format!(
                "{} WHERE workspace_id = ?1 ORDER BY created_at DESC, id",
                invite_select_sql()
            ))
            .context("failed to prepare invite query")?;
        statement
            .query_map([workspace_id], decode_invite)
            .context("failed to query invites")?
            .collect::<rusqlite::Result<Vec<_>>>()
            .context("failed to decode invites")
    }

    /// Lists every invite issued to one email address, across all
    /// workspaces, most recent first. Used to resolve which workspace a
    /// brand-new Google identity is joining, since the workspace isn't
    /// known until the invite is found.
    ///
    /// # Errors
    ///
    /// Returns an error when invite records cannot be read or decoded.
    pub fn invites_by_email(&self, email: &str) -> Result<Vec<InviteRecord>> {
        let mut statement = self
            .connection
            .prepare(&format!(
                // NOCASE to match user_by_email, which has always compared
                // this way. Addresses are lowercased before they are stored,
                // so the two agree on anything written by this code - but a
                // single row that ever escaped that normalization would be
                // found by one lookup and missed by the other, and the
                // symptom is an invite that visibly exists and still refuses
                // its holder as "not invited".
                "{} WHERE email = ?1 COLLATE NOCASE ORDER BY created_at DESC, id",
                invite_select_sql()
            ))
            .context("failed to prepare invite query")?;
        statement
            .query_map([email], decode_invite)
            .context("failed to query invites")?
            .collect::<rusqlite::Result<Vec<_>>>()
            .context("failed to decode invites")
    }

    /// Revokes a pending invite so its link can no longer be redeemed.
    ///
    /// # Errors
    ///
    /// Returns an error when the invite row cannot be updated.
    pub fn revoke_invite(&self, id: &str, now: &str) -> Result<bool> {
        self.connection
            .execute(
                "UPDATE invites SET revoked_at = ?2 WHERE id = ?1 AND accepted_at IS NULL",
                (id, now),
            )
            .map(|changed| changed == 1)
            .context("failed to revoke invite")
    }

    /// Marks an invite as accepted, so it cannot be redeemed again.
    ///
    /// # Errors
    ///
    /// Returns an error when the invite row cannot be updated.
    pub fn mark_invite_accepted(&self, id: &str, now: &str) -> Result<()> {
        self.connection
            .execute(
                "UPDATE invites SET accepted_at = ?2 WHERE id = ?1",
                (id, now),
            )
            .context("failed to mark invite accepted")?;
        Ok(())
    }

    /// Records one durable audit event.
    ///
    /// # Errors
    ///
    /// Returns an error when the event row cannot be written.
    pub fn record_audit_event(&self, event: &NewAuditEvent<'_>) -> Result<()> {
        self.connection
            .execute(
                "INSERT INTO audit_log (\
                   id, workspace_id, actor_type, actor_id, actor_label, action, \
                   target_type, target_id, detail_json, created_at\
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                params![
                    event.id,
                    event.workspace_id,
                    event.actor_type,
                    event.actor_id,
                    event.actor_label,
                    event.action,
                    event.target_type,
                    event.target_id,
                    event.detail_json,
                    event.created_at,
                ],
            )
            .context("failed to record audit event")?;
        Ok(())
    }

    /// Lists recent audit events for one workspace, most recent first.
    ///
    /// # Errors
    ///
    /// Returns an error when audit records cannot be read or decoded.
    pub fn audit_log_in_workspace(
        &self,
        workspace_id: &str,
        limit: u64,
    ) -> Result<Vec<AuditEventRecord>> {
        let limit = i64::try_from(limit).context("audit-log limit exceeds SQLite range")?;
        let mut statement = self
            .connection
            .prepare(
                "SELECT id, workspace_id, actor_type, actor_id, actor_label, action, \
                        target_type, target_id, detail_json, created_at \
                 FROM audit_log WHERE workspace_id = ?1 \
                 ORDER BY created_at DESC, id LIMIT ?2",
            )
            .context("failed to prepare audit-log query")?;
        statement
            .query_map((workspace_id, limit), decode_audit_event)
            .context("failed to query audit log")?
            .collect::<rusqlite::Result<Vec<_>>>()
            .context("failed to decode audit log")
    }

    /// Updates operator-editable database settings.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid settings, a missing database, or a storage
    /// failure.
    pub fn update_database(&self, id: &str, update: &DatabaseUpdate<'_>) -> Result<()> {
        validate_mode(update.mode)?;
        validate_keyless_policy(update.keyless_policy)?;
        let poll_interval = i64::try_from(update.poll_interval_seconds)
            .context("poll interval exceeds SQLite range")?;
        let reconcile_interval = i64::try_from(update.reconcile_interval_seconds)
            .context("reconcile interval exceeds SQLite range")?;
        let changed = self
            .connection
            .execute(
                "UPDATE databases SET \
                   name = ?2, \
                   mysql_dsn_encrypted = COALESCE(?3, mysql_dsn_encrypted), \
                   mode = ?4, include_tables = ?5, exclude_tables = ?6, \
                   poll_interval_seconds = ?7, reconcile_interval_seconds = ?8, \
                   updated_at = ?9, keyless_policy = ?10 \
                 WHERE id = ?1",
                params![
                    id,
                    update.name,
                    update.encrypted_dsn,
                    update.mode,
                    update.include_tables,
                    update.exclude_tables,
                    poll_interval,
                    reconcile_interval,
                    update.now,
                    update.keyless_policy,
                ],
            )
            .context("failed to update database")?;
        if changed == 0 {
            bail!("database {id} does not exist");
        }
        Ok(())
    }

    /// Persists the latest source probe and effective mode.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid mode, a missing database, or a storage
    /// failure.
    pub fn update_database_probe(
        &self,
        id: &str,
        probe_json: &str,
        effective_mode: &str,
        now: &str,
    ) -> Result<()> {
        if !matches!(effective_mode, "cdc" | "polling") {
            bail!("effective database mode must be cdc or polling");
        }
        let changed = self
            .connection
            .execute(
                "UPDATE databases SET probe_json = ?2, effective_mode = ?3, \
                   state = 'probed', updated_at = ?4 WHERE id = ?1",
                (id, probe_json, effective_mode, now),
            )
            .context("failed to persist database probe")?;
        if changed == 0 {
            bail!("database {id} does not exist");
        }
        Ok(())
    }

    /// Replaces only the stored probe report, leaving state and modes
    /// untouched. The CDC runner uses this when live DDL changes the source
    /// inventory (e.g. auto-including a newly created table): every consumer
    /// of `probe_json` — the supervisor's target set and the query engine's
    /// catalog — must see the new table without disturbing replication state.
    ///
    /// # Errors
    ///
    /// Returns an error for a missing database or a storage failure.
    pub fn refresh_database_probe_json(&self, id: &str, probe_json: &str, now: &str) -> Result<()> {
        let changed = self
            .connection
            .execute(
                "UPDATE databases SET probe_json = ?2, updated_at = ?3 WHERE id = ?1",
                (id, probe_json, now),
            )
            .context("failed to refresh database probe")?;
        if changed == 0 {
            bail!("database {id} does not exist");
        }
        Ok(())
    }

    /// Changes the requested replication mode.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid mode, a missing database, or a storage
    /// failure.
    pub fn set_database_mode(&self, id: &str, mode: &str, now: &str) -> Result<()> {
        validate_mode(mode)?;
        let changed = match mode {
            "paused" => self
                .connection
                .execute(
                    "UPDATE databases SET mode = 'paused', effective_mode = 'paused', \
                       state = 'paused', updated_at = ?2 WHERE id = ?1",
                    (id, now),
                )
                .context("failed to pause database")?,
            // `auto` means "follow the probe recommendation": a live
            // effective mode stays live until the next probe re-derives it,
            // while leaving pause via `auto` waits for that probe.
            "auto" => self
                .connection
                .execute(
                    "UPDATE databases SET mode = 'auto', \
                       effective_mode = CASE WHEN effective_mode = 'paused' \
                         THEN NULL ELSE effective_mode END, \
                       updated_at = ?2 WHERE id = ?1",
                    (id, now),
                )
                .context("failed to update database mode")?,
            // An explicit cdc/polling switch takes effect immediately and
            // must keep replication ALIVE: the supervisor only schedules
            // databases whose state is streaming/polling/error, so an
            // active (or paused) database transitions to the new mode's
            // running state instead of being reset to 'created' — that
            // reset silently stopped replication until a manual re-probe
            // (found by the e2e control-plane gate, 2026-08-03).
            explicit => self
                .connection
                .execute(
                    "UPDATE databases SET mode = ?2, effective_mode = ?2, \
                       state = CASE \
                         WHEN state IN ('streaming', 'polling', 'error', 'paused') \
                         THEN (CASE ?2 WHEN 'polling' THEN 'polling' ELSE 'streaming' END) \
                         ELSE state \
                       END, \
                       updated_at = ?3 WHERE id = ?1",
                    (id, explicit, now),
                )
                .context("failed to update database mode")?,
        };
        if changed == 0 {
            bail!("database {id} does not exist");
        }
        Ok(())
    }

    /// Publishes a completed snapshot handoff state.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid effective mode, a missing database, or
    /// a storage failure.
    pub fn set_database_replication_state(
        &self,
        id: &str,
        effective_mode: &str,
        now: &str,
    ) -> Result<()> {
        let (database_state, table_state) = match effective_mode {
            "cdc" => ("streaming", "streaming"),
            "polling" => ("polling", "polling"),
            _ => bail!("effective database mode must be cdc or polling"),
        };
        let transaction = self
            .connection
            .unchecked_transaction()
            .context("failed to begin replication-state update")?;
        let changed = transaction
            .execute(
                "UPDATE databases SET effective_mode = ?2, state = ?3, updated_at = ?4 \
                 WHERE id = ?1",
                (id, effective_mode, database_state, now),
            )
            .context("failed to update database replication state")?;
        if changed == 0 {
            bail!("database {id} does not exist");
        }
        transaction
            .execute(
                "UPDATE tables SET state = ?2, last_error = NULL \
                 WHERE db_id = ?1 AND state NOT IN ('excluded', 'needs_resync')",
                (id, table_state),
            )
            .context("failed to update table replication states")?;
        transaction
            .commit()
            .context("failed to commit replication-state update")
    }

    /// Records a database-level API job failure.
    ///
    /// # Errors
    ///
    /// Returns an error when the database cannot be updated.
    pub fn fail_database_job(&self, id: &str, error: &str, now: &str) -> Result<()> {
        let transaction = self
            .connection
            .unchecked_transaction()
            .context("failed to begin database failure update")?;
        let changed = transaction
            .execute(
                "UPDATE databases SET state = 'error', updated_at = ?2 WHERE id = ?1",
                (id, now),
            )
            .context("failed to record database job failure")?;
        if changed == 0 {
            bail!("database {id} does not exist");
        }
        transaction
            .execute(
                "UPDATE tables SET state = 'error', last_error = ?2 \
                 WHERE db_id = ?1 AND state NOT IN ('excluded', 'needs_resync')",
                (id, error),
            )
            .context("failed to record table job failure")?;
        transaction
            .commit()
            .context("failed to commit database failure update")
    }

    /// Deletes one database and its cascading control-plane records.
    ///
    /// # Errors
    ///
    /// Returns an error when the database record cannot be deleted.
    pub fn delete_database(&self, id: &str) -> Result<bool> {
        self.connection
            .execute("DELETE FROM databases WHERE id = ?1", [id])
            .map(|changed| changed == 1)
            .context("failed to delete database")
    }

    /// Lists durable table status for one database.
    ///
    /// # Errors
    ///
    /// Returns an error when table records cannot be read or decoded.
    pub fn tables(&self, database_id: &str) -> Result<Vec<TableRecord>> {
        let mut statement = self
            .connection
            .prepare(
                "SELECT db_id, name, state, pk_json, cursor_column, sort_key_json, \
                        rows_synced, last_error, last_reconcile_at, schema_version, \
                        orphaned_at, soft_delete_column \
                 FROM tables WHERE db_id = ?1 ORDER BY name COLLATE NOCASE",
            )
            .context("failed to prepare table query")?;
        statement
            .query_map([database_id], decode_table)
            .context("failed to query tables")?
            .collect::<rusqlite::Result<Vec<_>>>()
            .context("failed to decode tables")
    }

    /// Configures an optional source soft-delete column.
    ///
    /// # Errors
    ///
    /// Returns an error when the table is absent or cannot be updated.
    pub fn set_table_soft_delete_column(
        &self,
        database_id: &str,
        table_name: &str,
        column: Option<&str>,
    ) -> Result<()> {
        let changed = self
            .connection
            .execute(
                "UPDATE tables SET soft_delete_column = ?3 \
                 WHERE db_id = ?1 AND name = ?2",
                (database_id, table_name, column),
            )
            .context("failed to configure soft-delete column")?;
        if changed == 0 {
            bail!("table {database_id}.{table_name} does not exist");
        }
        Ok(())
    }

    /// Persists one hash-only database API key.
    ///
    /// # Errors
    ///
    /// Returns an error when the database is absent or key metadata cannot be
    /// stored.
    pub fn create_api_key(&self, key: &NewApiKey<'_>) -> Result<()> {
        self.connection
            .execute(
                "INSERT INTO api_keys (\
                   id, db_id, name, sha256, mysql_native_password_hash, \
                   caching_sha2_password_hash, scopes_json, expires_at, created_at\
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    key.id,
                    key.database_id,
                    key.name,
                    key.sha256,
                    key.mysql_native_password_hash,
                    key.caching_sha2_password_hash,
                    key.scopes_json,
                    key.expires_at,
                    key.now,
                ],
            )
            .context("failed to create API key")?;
        Ok(())
    }

    /// Lists API keys for one database without exposing their secret.
    ///
    /// # Errors
    ///
    /// Returns an error when API-key records cannot be read or decoded.
    pub fn api_keys(&self, database_id: &str) -> Result<Vec<ApiKeyRecord>> {
        let mut statement = self
            .connection
            .prepare(&format!(
                "{} WHERE db_id = ?1 ORDER BY created_at DESC, id",
                api_key_select_sql()
            ))
            .context("failed to prepare API-key query")?;
        statement
            .query_map([database_id], decode_api_key)
            .context("failed to query API keys")?
            .collect::<rusqlite::Result<Vec<_>>>()
            .context("failed to decode API keys")
    }

    /// Finds an API key by its SHA-256 digest.
    ///
    /// # Errors
    ///
    /// Returns an error when the API-key record cannot be read or decoded.
    pub fn api_key_by_sha256(&self, sha256: &[u8]) -> Result<Option<ApiKeyRecord>> {
        self.connection
            .query_row(
                &format!("{} WHERE sha256 = ?1", api_key_select_sql()),
                [sha256],
                decode_api_key,
            )
            .optional()
            .context("failed to read API key")
    }

    /// Enables or disables an API key.
    ///
    /// # Errors
    ///
    /// Returns an error when the key is absent or cannot be updated.
    pub fn set_api_key_enabled(&self, id: &str, enabled: bool) -> Result<()> {
        let changed = self
            .connection
            .execute(
                "UPDATE api_keys SET enabled = ?2 WHERE id = ?1",
                (id, enabled),
            )
            .context("failed to update API key")?;
        if changed == 0 {
            bail!("API key {id} does not exist");
        }
        Ok(())
    }

    /// Records successful API-key authentication.
    ///
    /// # Errors
    ///
    /// Returns an error when the key cannot be updated.
    pub fn touch_api_key(&self, id: &str, now: &str) -> Result<()> {
        self.connection
            .execute(
                "UPDATE api_keys SET last_used_at = ?2 WHERE id = ?1",
                (id, now),
            )
            .context("failed to update API-key usage")?;
        Ok(())
    }

    /// Deletes one API key.
    ///
    /// # Errors
    ///
    /// Returns an error when the key cannot be deleted.
    pub fn delete_api_key(&self, id: &str) -> Result<bool> {
        self.connection
            .execute("DELETE FROM api_keys WHERE id = ?1", [id])
            .map(|changed| changed == 1)
            .context("failed to delete API key")
    }

    /// Starts one durable activity record.
    ///
    /// # Errors
    ///
    /// Returns an error when the parent database is absent or the run cannot
    /// be stored.
    pub fn start_sync_run(
        &self,
        id: &str,
        database_id: &str,
        table_name: Option<&str>,
        kind: &str,
        now: &str,
    ) -> Result<()> {
        self.connection
            .execute(
                "INSERT INTO sync_runs (\
                   id, db_id, table_name, kind, status, started_at\
                 ) VALUES (?1, ?2, ?3, ?4, 'running', ?5)",
                (id, database_id, table_name, kind, now),
            )
            .context("failed to start sync run")?;
        Ok(())
    }

    /// Completes one durable activity record.
    ///
    /// # Errors
    ///
    /// Returns an error when counters exceed `SQLite`'s range, the run is
    /// absent, or it cannot be updated.
    pub fn finish_sync_run(
        &self,
        id: &str,
        status: &str,
        rows: u64,
        bytes: u64,
        duration_ms: u64,
        error: Option<&str>,
    ) -> Result<()> {
        let rows = i64::try_from(rows).context("sync rows exceed SQLite range")?;
        let bytes = i64::try_from(bytes).context("sync bytes exceed SQLite range")?;
        let duration = i64::try_from(duration_ms).context("sync duration exceeds SQLite range")?;
        let changed = self
            .connection
            .execute(
                "UPDATE sync_runs SET status = ?2, rows = ?3, bytes = ?4, \
                   duration_ms = ?5, error = ?6 WHERE id = ?1",
                (id, status, rows, bytes, duration, error),
            )
            .context("failed to complete sync run")?;
        if changed == 0 {
            bail!("sync run {id} does not exist");
        }
        Ok(())
    }

    /// Lists recent sync activity, optionally limited to one database.
    ///
    /// # Errors
    ///
    /// Returns an error when activity records cannot be read or decoded.
    pub fn sync_runs(&self, database_id: Option<&str>, limit: u64) -> Result<Vec<SyncRunRecord>> {
        let limit = i64::try_from(limit).context("sync-run limit exceeds SQLite range")?;
        let mut statement = self
            .connection
            .prepare(
                "SELECT id, db_id, table_name, kind, status, rows, bytes, duration_ms, \
                        error, started_at \
                 FROM sync_runs \
                 WHERE (?1 IS NULL OR db_id = ?1) \
                 ORDER BY started_at DESC, id LIMIT ?2",
            )
            .context("failed to prepare sync-run query")?;
        statement
            .query_map((database_id, limit), decode_sync_run)
            .context("failed to query sync runs")?
            .collect::<rusqlite::Result<Vec<_>>>()
            .context("failed to decode sync runs")
    }

    /// Lists recent activity for databases within one workspace, optionally
    /// narrowed to a single database in it.
    ///
    /// # Errors
    ///
    /// Returns an error when activity records cannot be read or decoded.
    pub fn sync_runs_in_workspace(
        &self,
        workspace_id: &str,
        database_id: Option<&str>,
        limit: u64,
    ) -> Result<Vec<SyncRunRecord>> {
        let limit = i64::try_from(limit).context("sync-run limit exceeds SQLite range")?;
        let mut statement = self
            .connection
            .prepare(
                "SELECT r.id, r.db_id, r.table_name, r.kind, r.status, r.rows, r.bytes, \
                        r.duration_ms, r.error, r.started_at \
                 FROM sync_runs r \
                 JOIN databases d ON d.id = r.db_id \
                 WHERE d.workspace_id = ?1 AND (?2 IS NULL OR r.db_id = ?2) \
                 ORDER BY r.started_at DESC, r.id LIMIT ?3",
            )
            .context("failed to prepare sync-run query")?;
        statement
            .query_map((workspace_id, database_id, limit), decode_sync_run)
            .context("failed to query sync runs")?
            .collect::<rusqlite::Result<Vec<_>>>()
            .context("failed to decode sync runs")
    }

    /// Lists recent dead-letter records, optionally limited to one database.
    ///
    /// # Errors
    ///
    /// Returns an error when dead-letter records cannot be read or decoded.
    pub fn dlq_records(&self, database_id: Option<&str>, limit: u64) -> Result<Vec<DlqRecord>> {
        let limit = i64::try_from(limit).context("DLQ limit exceeds SQLite range")?;
        let mut statement = self
            .connection
            .prepare(
                "SELECT id, db_id, table_name, event_json, error, created_at \
                 FROM dlq WHERE (?1 IS NULL OR db_id = ?1) \
                 ORDER BY created_at DESC, id LIMIT ?2",
            )
            .context("failed to prepare DLQ query")?;
        statement
            .query_map((database_id, limit), decode_dlq)
            .context("failed to query DLQ records")?
            .collect::<rusqlite::Result<Vec<_>>>()
            .context("failed to decode DLQ records")
    }

    /// Lists recent dead-letter records for databases within one workspace,
    /// optionally narrowed to a single database in it.
    ///
    /// # Errors
    ///
    /// Returns an error when dead-letter records cannot be read or decoded.
    pub fn dlq_records_in_workspace(
        &self,
        workspace_id: &str,
        database_id: Option<&str>,
        limit: u64,
    ) -> Result<Vec<DlqRecord>> {
        let limit = i64::try_from(limit).context("DLQ limit exceeds SQLite range")?;
        let mut statement = self
            .connection
            .prepare(
                "SELECT q.id, q.db_id, q.table_name, q.event_json, q.error, q.created_at \
                 FROM dlq q \
                 JOIN databases d ON d.id = q.db_id \
                 WHERE d.workspace_id = ?1 AND (?2 IS NULL OR q.db_id = ?2) \
                 ORDER BY q.created_at DESC, q.id LIMIT ?3",
            )
            .context("failed to prepare DLQ query")?;
        statement
            .query_map((workspace_id, database_id, limit), decode_dlq)
            .context("failed to query DLQ records")?
            .collect::<rusqlite::Result<Vec<_>>>()
            .context("failed to decode DLQ records")
    }

    /// Loads one dead-letter record by ID.
    ///
    /// # Errors
    ///
    /// Returns an error when the query or row decoding fails.
    pub fn dlq_record(&self, id: &str) -> Result<Option<DlqRecord>> {
        self.connection
            .query_row(
                "SELECT id, db_id, table_name, event_json, error, created_at \
                 FROM dlq WHERE id = ?1",
                [id],
                decode_dlq,
            )
            .optional()
            .context("failed to load DLQ record")
    }

    /// Discards one dead-letter record.
    ///
    /// # Errors
    ///
    /// Returns an error when the record cannot be deleted.
    pub fn delete_dlq_record(&self, id: &str) -> Result<bool> {
        self.connection
            .execute("DELETE FROM dlq WHERE id = ?1", [id])
            .map(|changed| changed == 1)
            .context("failed to delete DLQ record")
    }
}

fn invite_select_sql() -> &'static str {
    "SELECT id, workspace_id, email, role, created_by, created_at, expires_at, \
            accepted_at, revoked_at \
     FROM invites"
}

/// Consumes an invite as a compare-and-set, inside the caller's transaction.
///
/// Not a best-effort update. Every predicate that authorizes the admission is
/// re-checked here because the caller read them before the transaction began,
/// and between that read and this write the invite can be revoked or claimed
/// by someone else.
///
/// Requiring exactly one affected row is the point. An earlier version guarded
/// only on `accepted_at IS NULL` and discarded the count, so a missing or
/// already-consumed invite updated nothing while the user and the membership
/// committed anyway - admitting an account against an invite that no longer
/// authorized it.
fn claim_invite(
    transaction: &rusqlite::Transaction<'_>,
    admission: &GoogleAdmission<'_>,
) -> Result<()> {
    let claimed = transaction
        .execute(
            "UPDATE invites SET accepted_at = ?2 \
             WHERE id = ?1 \
               AND accepted_at IS NULL \
               AND revoked_at IS NULL \
               AND email = ?3 COLLATE NOCASE \
               AND workspace_id = ?4 \
               AND role = ?5",
            (
                admission.invite_id,
                admission.now,
                admission.email,
                admission.workspace_id,
                admission.role,
            ),
        )
        .context("failed to mark invite accepted")?;
    if claimed != 1 {
        bail!(
            "invite {} was not claimable: it is missing, already accepted, revoked, or no longer matches this email, workspace and role",
            admission.invite_id
        );
    }
    Ok(())
}

fn decode_invite(row: &rusqlite::Row<'_>) -> rusqlite::Result<InviteRecord> {
    Ok(InviteRecord {
        id: row.get(0)?,
        workspace_id: row.get(1)?,
        email: row.get(2)?,
        role: row.get(3)?,
        created_by: row.get(4)?,
        created_at: row.get(5)?,
        expires_at: row.get(6)?,
        accepted_at: row.get(7)?,
        revoked_at: row.get(8)?,
    })
}

fn decode_audit_event(row: &rusqlite::Row<'_>) -> rusqlite::Result<AuditEventRecord> {
    Ok(AuditEventRecord {
        id: row.get(0)?,
        workspace_id: row.get(1)?,
        actor_type: row.get(2)?,
        actor_id: row.get(3)?,
        actor_label: row.get(4)?,
        action: row.get(5)?,
        target_type: row.get(6)?,
        target_id: row.get(7)?,
        detail_json: row.get(8)?,
        created_at: row.get(9)?,
    })
}

fn decode_workspace(row: &rusqlite::Row<'_>) -> rusqlite::Result<WorkspaceRecord> {
    Ok(WorkspaceRecord {
        id: row.get(0)?,
        name: row.get(1)?,
        slug: row.get(2)?,
        created_at: row.get(3)?,
    })
}

fn decode_user(row: &rusqlite::Row<'_>) -> rusqlite::Result<UserRecord> {
    Ok(UserRecord {
        id: row.get(0)?,
        email: row.get(1)?,
        argon2_hash: row.get(2)?,
        role: row.get(3)?,
        enabled: row.get(4)?,
        created_at: row.get(5)?,
        last_login_at: row.get(6)?,
        google_subject: row.get(7)?,
    })
}

fn database_select_sql() -> &'static str {
    "SELECT id, name, mysql_dsn_encrypted, mode, effective_mode, state, probe_json, \
            include_tables, exclude_tables, poll_interval_seconds, \
            reconcile_interval_seconds, created_at, updated_at, keyless_policy, \
            kind, workspace_id \
     FROM databases"
}

fn decode_database(row: &rusqlite::Row<'_>) -> rusqlite::Result<DatabaseRecord> {
    let poll_interval: i64 = row.get(9)?;
    let reconcile_interval: i64 = row.get(10)?;
    Ok(DatabaseRecord {
        id: row.get(0)?,
        name: row.get(1)?,
        encrypted_dsn: row.get(2)?,
        mode: row.get(3)?,
        effective_mode: row.get(4)?,
        state: row.get(5)?,
        probe_json: row.get(6)?,
        include_tables: row.get(7)?,
        exclude_tables: row.get(8)?,
        poll_interval_seconds: u64::try_from(poll_interval).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                9,
                rusqlite::types::Type::Integer,
                Box::new(error),
            )
        })?,
        reconcile_interval_seconds: u64::try_from(reconcile_interval).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                10,
                rusqlite::types::Type::Integer,
                Box::new(error),
            )
        })?,
        created_at: row.get(11)?,
        updated_at: row.get(12)?,
        keyless_policy: row.get(13)?,
        kind: row.get(14)?,
        workspace_id: row.get(15)?,
    })
}

fn decode_table(row: &rusqlite::Row<'_>) -> rusqlite::Result<TableRecord> {
    let rows_synced: i64 = row.get(6)?;
    let schema_version: i64 = row.get(9)?;
    Ok(TableRecord {
        database_id: row.get(0)?,
        name: row.get(1)?,
        state: row.get(2)?,
        primary_key_json: row.get(3)?,
        cursor_column: row.get(4)?,
        sort_key_json: row.get(5)?,
        rows_synced: u64::try_from(rows_synced).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                6,
                rusqlite::types::Type::Integer,
                Box::new(error),
            )
        })?,
        last_error: row.get(7)?,
        last_reconcile_at: row.get(8)?,
        schema_version: u32::try_from(schema_version).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                9,
                rusqlite::types::Type::Integer,
                Box::new(error),
            )
        })?,
        orphaned_at: row.get(10)?,
        soft_delete_column: row.get(11)?,
    })
}

fn api_key_select_sql() -> &'static str {
    "SELECT id, db_id, name, sha256, mysql_native_password_hash, \
            caching_sha2_password_hash, enabled, \
            scopes_json, expires_at, last_used_at, created_at FROM api_keys"
}

fn decode_api_key(row: &rusqlite::Row<'_>) -> rusqlite::Result<ApiKeyRecord> {
    Ok(ApiKeyRecord {
        id: row.get(0)?,
        database_id: row.get(1)?,
        name: row.get(2)?,
        sha256: row.get(3)?,
        mysql_native_password_hash: row.get(4)?,
        caching_sha2_password_hash: row.get(5)?,
        enabled: row.get(6)?,
        scopes_json: row.get(7)?,
        expires_at: row.get(8)?,
        last_used_at: row.get(9)?,
        created_at: row.get(10)?,
    })
}

fn decode_sync_run(row: &rusqlite::Row<'_>) -> rusqlite::Result<SyncRunRecord> {
    let rows: i64 = row.get(5)?;
    let bytes: i64 = row.get(6)?;
    let duration: Option<i64> = row.get(7)?;
    Ok(SyncRunRecord {
        id: row.get(0)?,
        database_id: row.get(1)?,
        table_name: row.get(2)?,
        kind: row.get(3)?,
        status: row.get(4)?,
        rows: decode_u64(5, rows)?,
        bytes: decode_u64(6, bytes)?,
        duration_ms: duration.map(|value| decode_u64(7, value)).transpose()?,
        error: row.get(8)?,
        started_at: row.get(9)?,
    })
}

fn decode_dlq(row: &rusqlite::Row<'_>) -> rusqlite::Result<DlqRecord> {
    Ok(DlqRecord {
        id: row.get(0)?,
        database_id: row.get(1)?,
        table_name: row.get(2)?,
        event_json: row.get(3)?,
        error: row.get(4)?,
        created_at: row.get(5)?,
    })
}

fn decode_u64(index: usize, value: i64) -> rusqlite::Result<u64> {
    u64::try_from(value).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            index,
            rusqlite::types::Type::Integer,
            Box::new(error),
        )
    })
}

fn validate_role(role: &str) -> Result<()> {
    if matches!(role, "admin" | "operator" | "viewer") {
        Ok(())
    } else {
        bail!("user role must be admin, operator, or viewer")
    }
}

/// Rejects unknown keyless-table policies.
fn validate_keyless_policy(policy: &str) -> Result<()> {
    if !matches!(policy, "quarantine" | "auto_resync" | "reject") {
        bail!("keyless policy must be quarantine, auto_resync, or reject");
    }
    Ok(())
}

fn validate_mode(mode: &str) -> Result<()> {
    if matches!(mode, "auto" | "cdc" | "polling" | "paused") {
        Ok(())
    } else {
        bail!("database mode must be auto, cdc, polling, or paused")
    }
}
