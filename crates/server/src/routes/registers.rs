//! Registers (spec 2.1 §1, §2, §8).
//!
//! - Master data: `GET /registers[?branch_id=]`, `GET|PUT|DELETE /registers/{id}`. Read:
//!   `org.manage` or an assigned branch. Write: HQ only, `org.manage`. A register never moves to
//!   another branch (its code is in the branch's document numbers).
//! - Pairing: `POST /registers/{id}/pairing-code` (HQ only, `org.manage`) issues a one-time code,
//!   valid 10 minutes, and expires any earlier unused code of the register.
//!   `POST /registers/pair {code}` trades the code for a device token.
//!   `POST /registers/{id}/unpair` (HQ only, `org.manage`) revokes every device token and unused
//!   code of the register.
//! - `GET /registers/me`: the calling device's register and branch (code and name), e.g. for a
//!   terminal's Start screen.
//! - `GET /registers/me/users`: the lock-screen user list for the calling device.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sultan_core::access::Permission;
use sultan_core::ids::{BranchId, LocationId, RegisterId};
use sultan_db::audit::{self, AuditEntry};
use sultan_db::org::{self, Register};
use sultan_db::{Tx, devices, users};
use utoipa::ToSchema;

use super::locations::BranchFilter;
use super::{audit_entry, code, parse_id, parse_opt_id, parse_put_id, saved_status, text, to_json};
use crate::access::Caller;
use crate::error::ApiError;
use crate::extract::{ApiJson, ApiQuery, RegisterCtx, RequireHq, UserSession};
use crate::state::AppState;
use crate::token;

/// How long a pairing code is valid: 10 minutes (spec 2.1 §2).
pub const PAIRING_CODE_TTL_MS: i64 = 10 * 60 * 1_000;

/// Body of `POST /registers/pair`.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct PairRequest {
    /// The one-time pairing code shown in back office.
    pub code: String,
}

/// Answer of `POST /registers/pair`. The device token is shown once; the server keeps its hash.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct PairResponse {
    /// Send as `X-Register-Token` on every register request.
    pub device_token: String,
    /// The paired register.
    pub register_id: String,
    /// Its branch.
    pub branch_id: String,
    /// Register code, e.g. `R2`.
    pub register_code: String,
    /// Register display name.
    pub register_name: String,
    /// Branch code, e.g. `JKT01`.
    pub branch_code: String,
    /// Branch display name, e.g. `Toko Jakarta`.
    pub branch_name: String,
}

/// `POST /api/v1/registers/pair {code}` → `201` with a new device token (spec 2.1 §2).
///
/// The code is single use and valid until the `expires_at` set when it was issued (10 minutes).
/// Unknown, used or expired codes, and codes of an archived register, are `400
/// invalid_pairing_code`. Consuming the code, storing the token hash and the audit entry happen in
/// one transaction.
///
/// # Errors
/// See above, or `500 internal`.
#[utoipa::path(post, path = "/api/v1/registers/pair", tag = "registers",
    request_body = PairRequest,
    responses((status = 201, body = PairResponse), (status = 400, body = crate::openapi::Problem)))]
pub async fn pair(
    State(state): State<AppState>,
    ApiJson(request): ApiJson<PairRequest>,
) -> Result<(StatusCode, Json<PairResponse>), ApiError> {
    let now = state.now();
    let mut tx = state.db.begin().await?;
    let register_id = devices::consume_pairing_code(&mut tx, &token::hash(&request.code), now)
        .await?
        .ok_or_else(ApiError::invalid_pairing_code)?;
    let register = org::get_register(&mut tx, register_id)
        .await?
        .filter(|r| !r.archived)
        .ok_or_else(ApiError::invalid_pairing_code)?;
    let branch = org::get_branch(&mut tx, register.branch)
        .await?
        .ok_or_else(ApiError::invalid_pairing_code)?;
    let device_token = token::generate();
    devices::insert_device_token(&mut tx, &token::hash(&device_token), register.id, now).await?;
    let mut entry = AuditEntry::new(now, "register.pair");
    entry.branch = Some(register.branch);
    entry.register = Some(register.id);
    entry.target = Some(format!("register:{}", register.id));
    entry.after = Some(json!({ "register_code": register.code }));
    audit::append(&mut tx, &entry).await?;
    tx.commit().await?;
    Ok((
        StatusCode::CREATED,
        Json(PairResponse {
            device_token,
            register_id: register.id.to_string(),
            branch_id: register.branch.to_string(),
            register_code: register.code,
            register_name: register.name,
            branch_code: branch.code,
            branch_name: branch.name,
        }),
    ))
}

/// Answer of `GET /registers/me`: the calling device's register and its branch.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct RegisterMe {
    /// The register.
    pub register_id: String,
    /// Register code, e.g. `R2`.
    pub register_code: String,
    /// Register display name.
    pub register_name: String,
    /// Its branch.
    pub branch_id: String,
    /// Branch code, e.g. `JKT01`.
    pub branch_code: String,
    /// Branch display name, e.g. `Toko Jakarta`.
    pub branch_name: String,
}

/// `GET /api/v1/registers/me`: the register of the device token and its branch, so a terminal
/// can show (and refresh) what it is paired as.
///
/// # Errors
/// `401 invalid_device_token`, or `500 internal`.
#[utoipa::path(get, path = "/api/v1/registers/me", tag = "registers",
    security(("register_token" = [])),
    responses((status = 200, body = RegisterMe), (status = 401, body = crate::openapi::Problem)))]
pub async fn me(
    State(state): State<AppState>,
    ctx: RegisterCtx,
) -> Result<Json<RegisterMe>, ApiError> {
    let register = org::get_register(&state.db, ctx.register)
        .await?
        .ok_or_else(ApiError::invalid_device_token)?;
    let branch = org::get_branch(&state.db, ctx.branch)
        .await?
        .ok_or_else(ApiError::invalid_device_token)?;
    Ok(Json(RegisterMe {
        register_id: register.id.to_string(),
        register_code: register.code,
        register_name: register.name,
        branch_id: branch.id.to_string(),
        branch_code: branch.code,
        branch_name: branch.name,
    }))
}

/// One tile on the register lock screen.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct LockScreenUser {
    /// User ID, sent back with the PIN.
    pub id: String,
    /// Name on the tile.
    pub display_name: String,
}

/// `GET /api/v1/registers/me/users`: active users with a PIN and a role at the register's branch
/// (spec 2.1 §4). The branch comes from the device token.
///
/// # Errors
/// `401 invalid_device_token`, or `500 internal`.
#[utoipa::path(get, path = "/api/v1/registers/me/users", tag = "registers",
    security(("register_token" = [])),
    responses((status = 200, body = [LockScreenUser]), (status = 401, body = crate::openapi::Problem)))]
pub async fn me_users(
    State(state): State<AppState>,
    ctx: RegisterCtx,
) -> Result<Json<Vec<LockScreenUser>>, ApiError> {
    let users = users::list_for_branch(&state.db, ctx.branch).await?;
    Ok(Json(
        users
            .into_iter()
            .filter(|u| u.pin_hash.is_some())
            .map(|u| LockScreenUser {
                id: u.id.to_string(),
                display_name: u.display_name,
            })
            .collect(),
    ))
}

/// Body of `PUT /registers/{id}`.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct RegisterBody {
    /// The branch. Cannot change once created.
    pub branch_id: String,
    /// Code used in document numbers, e.g. `R2`: 1–10 letters or digits, unique per branch.
    pub code: String,
    /// Display name.
    pub name: String,
    /// Stock location sales are taken from; must belong to the same branch.
    #[serde(default)]
    pub default_location_id: Option<String>,
    /// `true` archives (its device tokens stop working); `false` (default) restores.
    #[serde(default)]
    pub archived: bool,
}

/// A register.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct RegisterDto {
    /// ID (`UUIDv7`).
    pub id: String,
    /// The branch.
    pub branch_id: String,
    /// Code, e.g. `R2`.
    pub code: String,
    /// Display name.
    pub name: String,
    /// Default stock location.
    pub default_location_id: Option<String>,
    /// Archived (soft-deleted).
    pub archived: bool,
}

impl From<&Register> for RegisterDto {
    fn from(r: &Register) -> Self {
        Self {
            id: r.id.to_string(),
            branch_id: r.branch.to_string(),
            code: r.code.clone(),
            name: r.name.clone(),
            default_location_id: r.default_location.map(|l| l.to_string()),
            archived: r.archived,
        }
    }
}

/// `GET /api/v1/registers`: registers of visible branches, including archived ones.
///
/// # Errors
/// `401 invalid_session`, `400 invalid_body` (bad `branch_id`).
#[utoipa::path(get, path = "/api/v1/registers", tag = "registers", security(("bearer" = [])),
    params(BranchFilter),
    responses((status = 200, body = [RegisterDto]), (status = 401, body = crate::openapi::Problem)))]
pub async fn list(
    State(state): State<AppState>,
    session: UserSession,
    ApiQuery(filter): ApiQuery<BranchFilter>,
) -> Result<Json<Vec<RegisterDto>>, ApiError> {
    let branch: Option<BranchId> = parse_opt_id(filter.branch_id.as_deref(), "branch_id")?;
    let view = Caller::load(&state.db, &session).await?.branch_view();
    let registers = org::list_registers(&state.db, branch).await?;
    Ok(Json(
        registers
            .iter()
            .filter(|r| view.allows(r.branch))
            .map(RegisterDto::from)
            .collect(),
    ))
}

/// `GET /api/v1/registers/{id}`. A register of an invisible branch is `404`.
///
/// # Errors
/// `401 invalid_session`, `400 invalid_body`, `404 not_found`.
#[utoipa::path(get, path = "/api/v1/registers/{id}", tag = "registers", security(("bearer" = [])),
    params(("id" = String, Path, description = "Register ID")),
    responses((status = 200, body = RegisterDto), (status = 404, body = crate::openapi::Problem)))]
pub async fn get(
    State(state): State<AppState>,
    session: UserSession,
    Path(id): Path<String>,
) -> Result<Json<RegisterDto>, ApiError> {
    let id: RegisterId = parse_id(&id, "id")?;
    let view = Caller::load(&state.db, &session).await?.branch_view();
    org::get_register(&state.db, id)
        .await?
        .filter(|r| view.allows(r.branch))
        .map(|r| Json(RegisterDto::from(&r)))
        .ok_or_else(|| ApiError::not_found("register"))
}

/// `422 invalid_field` unless `branch` exists and equals the record's current branch (if any).
pub(super) async fn ensure_branch(
    tx: &mut Tx,
    branch: BranchId,
    current: Option<BranchId>,
) -> Result<(), ApiError> {
    if current.is_some_and(|c| c != branch) {
        return Err(ApiError::invalid_field(
            "branch_id",
            "`branch_id` cannot change",
        ));
    }
    if org::get_branch(&mut *tx, branch).await?.is_none() {
        return Err(ApiError::invalid_field("branch_id", "unknown branch"));
    }
    Ok(())
}

/// `PUT /api/v1/registers/{id}`: create (`201`) or overwrite (`200`); a replay changes nothing.
///
/// # Errors
/// `403 read_only_replica` / `permission_denied`, `400 invalid_body`, `422 invalid_field`
/// (unknown branch, branch changed, location of another branch, bad code), `409 conflict`
/// (duplicate code in the branch).
#[utoipa::path(put, path = "/api/v1/registers/{id}", tag = "registers", security(("bearer" = [])),
    params(("id" = String, Path, description = "Client-generated UUIDv7")),
    request_body = RegisterBody,
    responses((status = 201, body = RegisterDto), (status = 200, body = RegisterDto),
        (status = 403, body = crate::openapi::Problem), (status = 409, body = crate::openapi::Problem),
        (status = 422, body = crate::openapi::Problem)))]
pub async fn put(
    State(state): State<AppState>,
    _hq: RequireHq,
    session: UserSession,
    Path(id): Path<String>,
    ApiJson(body): ApiJson<RegisterBody>,
) -> Result<(StatusCode, Json<RegisterDto>), ApiError> {
    let id: RegisterId = parse_put_id(&id)?;
    Caller::load(&state.db, &session)
        .await?
        .require(Permission::OrgManage)?;
    let branch: BranchId = parse_id(&body.branch_id, "branch_id")?;
    let location: Option<LocationId> =
        parse_opt_id(body.default_location_id.as_deref(), "default_location_id")?;
    let code = code("code", &body.code)?;
    let name = text("name", &body.name, 100)?;

    let mut tx = state.db.begin().await?;
    let before = org::get_register(&mut tx, id).await?;
    ensure_branch(&mut tx, branch, before.as_ref().map(|r| r.branch)).await?;
    if let Some(location) = location {
        let same_branch = org::get_location(&mut tx, location)
            .await?
            .is_some_and(|l| l.branch == branch);
        if !same_branch {
            return Err(ApiError::invalid_field(
                "default_location_id",
                "unknown location or location of another branch",
            ));
        }
    }
    let after = Register {
        id,
        branch,
        code,
        name,
        default_location: location,
        archived: body.archived,
    };
    let status = save(tx, &state, &session, before, &after, "register.save").await?;
    Ok((status, Json(RegisterDto::from(&after))))
}

/// `DELETE /api/v1/registers/{id}`: archive (soft delete). The register's device tokens stop
/// working at once.
///
/// # Errors
/// `403 read_only_replica` / `permission_denied`, `404 not_found`.
#[utoipa::path(delete, path = "/api/v1/registers/{id}", tag = "registers", security(("bearer" = [])),
    params(("id" = String, Path, description = "Register ID")),
    responses((status = 200, body = RegisterDto), (status = 404, body = crate::openapi::Problem)))]
pub async fn archive(
    State(state): State<AppState>,
    _hq: RequireHq,
    session: UserSession,
    Path(id): Path<String>,
) -> Result<Json<RegisterDto>, ApiError> {
    let id: RegisterId = parse_id(&id, "id")?;
    Caller::load(&state.db, &session)
        .await?
        .require(Permission::OrgManage)?;
    let mut tx = state.db.begin().await?;
    let before = org::get_register(&mut tx, id)
        .await?
        .ok_or_else(|| ApiError::not_found("register"))?;
    let after = Register {
        archived: true,
        ..before.clone()
    };
    save(
        tx,
        &state,
        &session,
        Some(before),
        &after,
        "register.archive",
    )
    .await?;
    Ok(Json(RegisterDto::from(&after)))
}

/// Saves `after` with its audit entry and outbox event, unless nothing changed.
async fn save(
    mut tx: Tx,
    state: &AppState,
    session: &UserSession,
    before: Option<Register>,
    after: &Register,
    action: &str,
) -> Result<StatusCode, ApiError> {
    if before.as_ref() == Some(after) {
        return Ok(StatusCode::OK);
    }
    let now = state.now();
    let saved = org::save_register(&mut tx, after, now).await?;
    let entry = audit_entry(
        session,
        now,
        action,
        format!("register:{}", after.id),
        before.as_ref().map(|r| to_json(&RegisterDto::from(r))),
        Some(to_json(&RegisterDto::from(after))),
    );
    audit::append(&mut tx, &entry).await?;
    tx.commit().await?;
    Ok(saved_status(saved))
}

/// A new pairing code. Shown once; the server keeps its hash.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct PairingCodeResponse {
    /// Type this into the register's setup wizard (`POST /registers/pair`).
    pub code: String,
    /// The register it pairs.
    pub register_id: String,
    /// Valid until (unix ms), 10 minutes from now.
    pub expires_at: i64,
}

/// Loads a non-archived register for an HQ register action, after the permission check.
async fn active_register(tx: &mut Tx, id: &str) -> Result<Register, ApiError> {
    let id: RegisterId = parse_id(id, "id")?;
    org::get_register(&mut *tx, id)
        .await?
        .filter(|r| !r.archived)
        .ok_or_else(|| ApiError::not_found("register"))
}

/// `POST /api/v1/registers/{id}/pairing-code` → `201` with a one-time code (spec 2.1 §2).
///
/// Any earlier unused code of the register stops working. Audited as `register.pairing_code`
/// (the code itself is not logged).
///
/// # Errors
/// `403 read_only_replica` / `permission_denied`, `404 not_found` (unknown or archived register).
#[utoipa::path(post, path = "/api/v1/registers/{id}/pairing-code", tag = "registers",
    security(("bearer" = [])), params(("id" = String, Path, description = "Register ID")),
    responses((status = 201, body = PairingCodeResponse), (status = 403, body = crate::openapi::Problem),
        (status = 404, body = crate::openapi::Problem)))]
pub async fn pairing_code(
    State(state): State<AppState>,
    _hq: RequireHq,
    session: UserSession,
    Path(id): Path<String>,
) -> Result<(StatusCode, Json<PairingCodeResponse>), ApiError> {
    Caller::load(&state.db, &session)
        .await?
        .require(Permission::OrgManage)?;
    let now = state.now();
    let expires_at = now.saturating_add_millis(PAIRING_CODE_TTL_MS);
    let mut tx = state.db.begin().await?;
    let register = active_register(&mut tx, &id).await?;
    let replaced = devices::expire_pairing_codes(&mut tx, register.id, now).await?;
    let code = token::pairing_code();
    devices::create_pairing_code(&mut tx, &token::hash(&code), register.id, now, expires_at)
        .await?;
    let entry = audit_entry(
        &session,
        now,
        "register.pairing_code",
        format!("register:{}", register.id),
        None,
        Some(json!({
            "expires_at": expires_at.unix_millis(),
            "replaced_codes": replaced,
        })),
    );
    audit::append(&mut tx, &entry).await?;
    tx.commit().await?;
    Ok((
        StatusCode::CREATED,
        Json(PairingCodeResponse {
            code,
            register_id: register.id.to_string(),
            expires_at: expires_at.unix_millis(),
        }),
    ))
}

/// Answer of `POST /registers/{id}/unpair`.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct UnpairResponse {
    /// How many active device tokens were revoked.
    pub revoked_tokens: u64,
}

/// `POST /api/v1/registers/{id}/unpair`: revokes every device token and unused pairing code of
/// the register (spec 2.1 §2). Sessions bound to the register stop working with the token.
/// Audited as `register.unpair`.
///
/// # Errors
/// `403 read_only_replica` / `permission_denied`, `404 not_found`.
#[utoipa::path(post, path = "/api/v1/registers/{id}/unpair", tag = "registers",
    security(("bearer" = [])), params(("id" = String, Path, description = "Register ID")),
    responses((status = 200, body = UnpairResponse), (status = 403, body = crate::openapi::Problem),
        (status = 404, body = crate::openapi::Problem)))]
pub async fn unpair(
    State(state): State<AppState>,
    _hq: RequireHq,
    session: UserSession,
    Path(id): Path<String>,
) -> Result<Json<UnpairResponse>, ApiError> {
    Caller::load(&state.db, &session)
        .await?
        .require(Permission::OrgManage)?;
    let now = state.now();
    let mut tx = state.db.begin().await?;
    let register = active_register(&mut tx, &id).await?;
    let revoked_tokens = devices::revoke_register_tokens(&mut tx, register.id, now).await?;
    devices::expire_pairing_codes(&mut tx, register.id, now).await?;
    let entry = audit_entry(
        &session,
        now,
        "register.unpair",
        format!("register:{}", register.id),
        None,
        Some(json!({ "revoked_tokens": revoked_tokens })),
    );
    audit::append(&mut tx, &entry).await?;
    tx.commit().await?;
    Ok(Json(UnpairResponse { revoked_tokens }))
}
