//! Users (spec 2.1 §3, §4, §8).
//!
//! - `GET /users`, `GET /users/{id}`: `user.manage`.
//! - `PUT /users/{id}`, `DELETE /users/{id}`: HQ only, `user.manage`. The body carries the role
//!   assignments (role + branch, or all branches when `branch_id` is null). `pin` / `password`
//!   set a new secret (validated and hashed with argon2id); when omitted the current one is kept.
//!   Hashes are never returned: responses say only `has_pin` / `has_password`.
//! - `DELETE` archives a user by deactivating them: they cannot log in, and their existing
//!   sessions stop working on the next request (users have no separate archived flag).
//! - Every write checks the Owner invariant inside the transaction (spec 2.1 §3, §9 case 6):
//!   deactivating, archiving or unassigning the last active Owner is `409 last_owner`, and
//!   nothing (row, audit entry, outbox event) is kept.
//! - **Delegation** (spec 2.1 §3, plan Phase 6 fix): a caller may assign or unassign a role only
//!   if they hold every grant of that role, with limits at least as high, at the branch scope
//!   being assigned (`core::access::ensure_can_grant`); changing anything else about an existing
//!   user (credentials, activation, names) or archiving them additionally needs the caller to
//!   cover every role the user holds now, so nobody can take over or lock out a more powerful
//!   account. Otherwise `403 permission_denied` with `reason: grant_exceeds_own` and the
//!   `permission`. An Owner for all branches passes trivially.
//! - Deactivating or archiving a user revokes their login sessions (`session.revoked` is pushed
//!   to their event sockets); every change pushes `user.changed` (plan Phase 6).
//! - `POST /users/{id}/unlock`: `user.unlock` at the session's branch (Phase 4).

use std::collections::BTreeSet;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sultan_core::access::{BranchScope, Permission};
use sultan_core::ids::{BranchId, RoleId, UserId};
use sultan_db::audit::{self, AuditEntry};
use sultan_db::users::{self, User};
use sultan_db::{Tx, auth, org, roles};
use utoipa::ToSchema;

use super::{
    attempts_json, audit_entry, parse_id, parse_opt_id, parse_put_id, saved_status, text, to_json,
};
use crate::access::{Caller, ensure_owner_remains};
use crate::credentials;
use crate::error::ApiError;
use crate::events::Event;
use crate::extract::{ApiJson, RequireHq, UserSession};
use crate::state::AppState;

/// A role given to a user at one branch, or at all branches when `branch_id` is null
/// (spec 2.1 §3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct AssignmentDto {
    /// The role.
    pub role_id: String,
    /// The branch, or `null` for all branches.
    #[serde(default)]
    pub branch_id: Option<String>,
}

const fn yes() -> bool {
    true
}

/// Body of `PUT /users/{id}`.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct UserBody {
    /// Back-office login name, unique.
    pub username: String,
    /// Name on the register lock screen.
    pub display_name: String,
    /// `false` deactivates the user. Default `true`.
    #[serde(default = "yes")]
    pub active: bool,
    /// New PIN (4–6 digits). Omit to keep the current one.
    #[serde(default)]
    pub pin: Option<String>,
    /// New password (at least 8 characters). Omit to keep the current one.
    #[serde(default)]
    pub password: Option<String>,
    /// Replaces all role assignments. Omit to keep the current ones.
    #[serde(default)]
    pub assignments: Option<Vec<AssignmentDto>>,
}

/// A user. Credential hashes are never returned.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct UserDto {
    /// ID (`UUIDv7`).
    pub id: String,
    /// Login name.
    pub username: String,
    /// Display name.
    pub display_name: String,
    /// Active (inactive = archived or deactivated).
    pub active: bool,
    /// Whether a PIN is set.
    pub has_pin: bool,
    /// Whether a password is set.
    pub has_password: bool,
    /// Role assignments, ordered by role then branch.
    pub assignments: Vec<AssignmentDto>,
}

/// Assignments in a canonical order, so a replay compares equal.
type Assignments = Vec<(RoleId, Option<BranchId>)>;

fn user_dto(user: &User, assignments: &Assignments) -> UserDto {
    UserDto {
        id: user.id.to_string(),
        username: user.username.clone(),
        display_name: user.display_name.clone(),
        active: user.active,
        has_pin: user.pin_hash.is_some(),
        has_password: user.password_hash.is_some(),
        assignments: assignments
            .iter()
            .map(|(role, branch)| AssignmentDto {
                role_id: role.to_string(),
                branch_id: branch.map(|b| b.to_string()),
            })
            .collect(),
    }
}

fn canonical(mut assignments: Assignments) -> Assignments {
    assignments.sort_unstable();
    assignments.dedup();
    assignments
}

const fn scope(branch: Option<BranchId>) -> BranchScope {
    match branch {
        None => BranchScope::All,
        Some(b) => BranchScope::Branch(b),
    }
}

const fn branch_of(scope: BranchScope) -> Option<BranchId> {
    match scope {
        BranchScope::All => None,
        BranchScope::Branch(b) => Some(b),
    }
}

async fn load_assignments(tx: &mut Tx, user: UserId) -> Result<Assignments, ApiError> {
    let stored = roles::assignments_for_user(&mut *tx, user).await?;
    Ok(canonical(
        stored
            .iter()
            .map(|a| (a.role, branch_of(a.scope)))
            .collect(),
    ))
}

/// `GET /api/v1/users`: every user (including inactive ones) by username. Needs `user.manage`.
///
/// # Errors
/// `401 invalid_session`, `403 permission_denied`.
#[utoipa::path(get, path = "/api/v1/users", tag = "users", security(("bearer" = [])),
    responses((status = 200, body = [UserDto]), (status = 403, body = crate::openapi::Problem)))]
pub async fn list(
    State(state): State<AppState>,
    session: UserSession,
) -> Result<Json<Vec<UserDto>>, ApiError> {
    Caller::load(&state.db, &session)
        .await?
        .require(Permission::UserManage)?;
    let all = users::list_users(&state.db).await?;
    let assignments = roles::list_assignments(&state.db).await?;
    Ok(Json(
        all.iter()
            .map(|u| {
                let mine = assignments
                    .iter()
                    .filter(|a| a.user == u.id)
                    .map(|a| (a.role, branch_of(a.scope)))
                    .collect();
                user_dto(u, &canonical(mine))
            })
            .collect(),
    ))
}

/// `GET /api/v1/users/{id}`. Needs `user.manage`.
///
/// # Errors
/// `401 invalid_session`, `403 permission_denied`, `400 invalid_body`, `404 not_found`.
#[utoipa::path(get, path = "/api/v1/users/{id}", tag = "users", security(("bearer" = [])),
    params(("id" = String, Path, description = "User ID")),
    responses((status = 200, body = UserDto), (status = 403, body = crate::openapi::Problem),
        (status = 404, body = crate::openapi::Problem)))]
pub async fn get(
    State(state): State<AppState>,
    session: UserSession,
    Path(id): Path<String>,
) -> Result<Json<UserDto>, ApiError> {
    Caller::load(&state.db, &session)
        .await?
        .require(Permission::UserManage)?;
    let id: UserId = parse_id(&id, "id")?;
    let mut tx = state.db.begin_read().await?;
    let user = users::get_user(&mut tx, id)
        .await?
        .ok_or_else(|| ApiError::not_found("user"))?;
    let assignments = load_assignments(&mut tx, id).await?;
    tx.commit().await?;
    Ok(Json(user_dto(&user, &assignments)))
}

/// Runs a CPU-heavy hash on the blocking pool.
async fn hash_blocking(
    hash: fn(&str) -> Result<String, ApiError>,
    secret: String,
) -> Result<String, ApiError> {
    tokio::task::spawn_blocking(move || hash(&secret))
        .await
        .map_err(|_| ApiError::internal())?
}

/// The stored hash for a secret field: the current hash when omitted or unchanged (so a replay
/// is a no-op), otherwise a fresh hash of the validated new value.
async fn new_hash(
    current: Option<String>,
    secret: Option<String>,
    validate: fn(&str) -> Result<(), ApiError>,
    hash: fn(&str) -> Result<String, ApiError>,
) -> Result<Option<String>, ApiError> {
    let Some(secret) = secret else {
        return Ok(current);
    };
    validate(&secret)?;
    if let Some(phc) = current
        && credentials::verify_blocking(phc.clone(), secret.clone()).await?
    {
        return Ok(Some(phc));
    }
    hash_blocking(hash, secret).await.map(Some)
}

/// Parses and checks the requested assignments: every role must exist (not archived) and every
/// branch must exist.
async fn parse_assignments(
    tx: &mut Tx,
    requested: &[AssignmentDto],
) -> Result<Assignments, ApiError> {
    let mut parsed = Vec::with_capacity(requested.len());
    for a in requested {
        let role: RoleId = parse_id(&a.role_id, "role_id")?;
        let branch: Option<BranchId> = parse_opt_id(a.branch_id.as_deref(), "branch_id")?;
        if roles::get_role(&mut *tx, role).await?.is_none() {
            return Err(ApiError::invalid_field("assignments", "unknown role"));
        }
        if let Some(b) = branch
            && org::get_branch(&mut *tx, b).await?.is_none()
        {
            return Err(ApiError::invalid_field("assignments", "unknown branch"));
        }
        parsed.push((role, branch));
    }
    Ok(canonical(parsed))
}

/// `PUT /api/v1/users/{id}`: create (`201`) or overwrite (`200`); a replay changes nothing.
///
/// # Errors
/// `403 read_only_replica` / `permission_denied`, `400 invalid_body`, `422 invalid_field` /
/// `invalid_pin` / `weak_password`, `409 conflict` (duplicate username) / `last_owner`.
#[utoipa::path(put, path = "/api/v1/users/{id}", tag = "users", security(("bearer" = [])),
    params(("id" = String, Path, description = "Client-generated UUIDv7")),
    request_body = UserBody,
    responses((status = 201, body = UserDto), (status = 200, body = UserDto),
        (status = 403, body = crate::openapi::Problem), (status = 409, body = crate::openapi::Problem),
        (status = 422, body = crate::openapi::Problem)))]
pub async fn put(
    State(state): State<AppState>,
    _hq: RequireHq,
    session: UserSession,
    Path(id): Path<String>,
    ApiJson(body): ApiJson<UserBody>,
) -> Result<(StatusCode, Json<UserDto>), ApiError> {
    let id: UserId = parse_put_id(&id)?;
    let caller = Caller::load(&state.db, &session).await?;
    caller.require(Permission::UserManage)?;
    let username = text("username", &body.username, 64)?;
    let display_name = text("display_name", &body.display_name, 100)?;

    // Hashing is slow, so it happens before the transaction starts.
    let current = users::get_user(&state.db, id).await?;
    let (current_pin, current_password) = current
        .map(|u| (u.pin_hash, u.password_hash))
        .unwrap_or_default();
    let pin_hash = new_hash(
        current_pin.clone(),
        body.pin,
        credentials::validate_pin,
        credentials::hash_pin,
    )
    .await?;
    let password_hash = new_hash(
        current_password.clone(),
        body.password,
        credentials::validate_password,
        credentials::hash_password,
    )
    .await?;

    let mut tx = state.db.begin().await?;
    let before = users::get_user(&mut tx, id).await?;
    let before_assignments = load_assignments(&mut tx, id).await?;
    let after_assignments = match &body.assignments {
        Some(requested) => parse_assignments(&mut tx, requested).await?,
        None => before_assignments.clone(),
    };
    let after = User {
        id,
        username,
        display_name,
        pin_hash,
        password_hash,
        active: body.active,
    };
    let changed = Changes {
        pin: after.pin_hash != current_pin,
        password: after.password_hash != current_password,
    };
    let record_changed = before.as_ref().is_some_and(|b| b != &after);
    ensure_delegation(
        &mut tx,
        &caller,
        record_changed,
        &before_assignments,
        &after_assignments,
    )
    .await?;
    let dto = user_dto(&after, &after_assignments);
    let status = save(
        tx,
        &state,
        &session,
        (before, &before_assignments),
        (&after, &after_assignments),
        changed,
        "user.save",
    )
    .await?;
    Ok((status, Json(dto)))
}

/// `DELETE /api/v1/users/{id}`: archive by deactivating; assignments are kept for history.
///
/// # Errors
/// `403 read_only_replica` / `permission_denied`, `404 not_found`, `409 last_owner`.
#[utoipa::path(delete, path = "/api/v1/users/{id}", tag = "users", security(("bearer" = [])),
    params(("id" = String, Path, description = "User ID")),
    responses((status = 200, body = UserDto), (status = 404, body = crate::openapi::Problem),
        (status = 409, body = crate::openapi::Problem)))]
pub async fn archive(
    State(state): State<AppState>,
    _hq: RequireHq,
    session: UserSession,
    Path(id): Path<String>,
) -> Result<Json<UserDto>, ApiError> {
    let id: UserId = parse_id(&id, "id")?;
    let caller = Caller::load(&state.db, &session).await?;
    caller.require(Permission::UserManage)?;
    let mut tx = state.db.begin().await?;
    let before = users::get_user(&mut tx, id)
        .await?
        .ok_or_else(|| ApiError::not_found("user"))?;
    let assignments = load_assignments(&mut tx, id).await?;
    ensure_delegation(&mut tx, &caller, true, &assignments, &assignments).await?;
    let after = User {
        active: false,
        ..before.clone()
    };
    let dto = user_dto(&after, &assignments);
    save(
        tx,
        &state,
        &session,
        (Some(before), &assignments),
        (&after, &assignments),
        Changes::default(),
        "user.archive",
    )
    .await?;
    Ok(Json(dto))
}

/// The delegation rule (see the module docs): every assignment added or removed, and — when the
/// user record itself changes — every assignment the user holds now, must be covered by the
/// caller's permissions at that assignment's scope. Archived roles grant nothing and are skipped.
async fn ensure_delegation(
    tx: &mut Tx,
    caller: &Caller,
    record_changed: bool,
    before: &Assignments,
    after: &Assignments,
) -> Result<(), ApiError> {
    let before_set: BTreeSet<_> = before.iter().copied().collect();
    let after_set: BTreeSet<_> = after.iter().copied().collect();
    let mut to_check: BTreeSet<_> = before_set
        .symmetric_difference(&after_set)
        .copied()
        .collect();
    if record_changed {
        to_check.extend(before_set);
    }
    for (role, branch) in to_check {
        if let Some(role) = roles::get_role(&mut *tx, role).await? {
            caller.ensure_can_grant(scope(branch), &role.grants)?;
        }
    }
    Ok(())
}

/// Which secrets a write replaced (recorded in the audit entry; the hashes are not).
#[derive(Debug, Clone, Copy, Default)]
struct Changes {
    pin: bool,
    password: bool,
}

/// Saves the user and their assignments with audit entry and outbox events, unless nothing
/// changed, then checks the Owner invariant before committing.
async fn save(
    mut tx: Tx,
    state: &AppState,
    session: &UserSession,
    (before, before_assignments): (Option<User>, &Assignments),
    (after, after_assignments): (&User, &Assignments),
    changed: Changes,
    action: &str,
) -> Result<StatusCode, ApiError> {
    let user_changed = before.as_ref() != Some(after);
    let assignments_changed = before_assignments != after_assignments;
    if !user_changed && !assignments_changed {
        return Ok(StatusCode::OK);
    }
    let now = state.now();
    if user_changed {
        users::save_user(&mut tx, after, now).await?;
    }
    let deactivated = before.as_ref().is_some_and(|b| b.active) && !after.active;
    if deactivated {
        auth::revoke_user_sessions(&mut tx, after.id, now).await?;
    }
    if assignments_changed {
        let scoped: Vec<(RoleId, BranchScope)> = after_assignments
            .iter()
            .map(|(role, branch)| (*role, scope(*branch)))
            .collect();
        roles::replace_assignments(&mut tx, after.id, &scoped, now).await?;
    }
    ensure_owner_remains(&mut tx).await?;

    let mut after_json = to_json(&user_dto(after, after_assignments));
    if let Value::Object(fields) = &mut after_json {
        fields.insert("pin_changed".into(), changed.pin.into());
        fields.insert("password_changed".into(), changed.password.into());
    }
    let entry = audit_entry(
        session,
        now,
        action,
        format!("user:{}", after.id),
        before
            .as_ref()
            .map(|b| to_json(&user_dto(b, before_assignments))),
        Some(after_json),
    );
    audit::append(&mut tx, &entry).await?;
    tx.commit().await?;
    if deactivated {
        state
            .events
            .publish(Event::session_revoked(after.id, None, "user_deactivated"));
    }
    let scopes = before_assignments
        .iter()
        .chain(after_assignments)
        .map(|(_, branch)| scope(*branch))
        .collect();
    state.events.publish(Event::user_changed(after.id, scopes));
    Ok(saved_status(if before.is_some() {
        sultan_db::Saved::Updated
    } else {
        sultan_db::Saved::Created
    }))
}

/// `POST /api/v1/users/{id}/unlock` → `204`: clears the lockout of user `id` at the caller's
/// session branch (spec 2.1 §4). Needs `user.unlock` there. Lockout state is per branch node, not
/// master data, so this works on branch nodes too. Audited as `user.unlock` (spec 2.1 §7).
///
/// # Errors
/// `401 invalid_session`, `403 permission_denied`, `400 invalid_body` (bad ID), `404 not_found`.
#[utoipa::path(post, path = "/api/v1/users/{id}/unlock", tag = "users", security(("bearer" = [])),
    params(("id" = String, Path, description = "User ID")),
    responses((status = 204), (status = 403, body = crate::openapi::Problem),
        (status = 404, body = crate::openapi::Problem)))]
pub async fn unlock(
    State(state): State<AppState>,
    session: UserSession,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    session
        .permissions(&state.db)
        .await?
        .require(Permission::UserUnlock, None)?;
    let target: UserId = parse_id(&id, "id")?;
    users::get_user(&state.db, target)
        .await?
        .ok_or_else(|| ApiError::not_found("user"))?;

    let now = state.now();
    let branch = session.session.branch;
    let mut tx = state.db.begin().await?;
    let before = auth::load_login_attempts(&mut tx, target, branch).await?;
    let mut after = before;
    after.unlock();
    auth::save_login_attempts(&mut tx, target, branch, &after).await?;
    let mut entry = AuditEntry::new(now, "user.unlock");
    entry.branch = Some(branch);
    entry.register = session.session.register;
    entry.user = Some(session.user.id);
    entry.target = Some(format!("user:{target}"));
    entry.before = Some(attempts_json(&before));
    entry.after = Some(attempts_json(&after));
    audit::append(&mut tx, &entry).await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}
