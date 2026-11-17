//! Held carts (spec 2.6 §4; phase S3): a cart put aside on the branch server and resumed on any
//! register of the branch. Branch-local, not synced, not documents, never dropped automatically.
//!
//! - `PUT /held-carts/{id} {customer_id?, note?, payload, item_count, total}`: hold (or hold
//!   again) the register's cart. A PIN session at the register with `sale.create`. `payload` is
//!   the cart as the register keeps it (opaque JSON). `201` on create, `200` when held again.
//!   Publishes `cart.held` (branch audience).
//! - `GET /held-carts`: the branch's waiting carts, newest first (any session of the branch).
//! - `POST /held-carts/{id}/take` with `Idempotency-Key`: resume it; `sale.create`. A cart taken
//!   already (also by a concurrent register) → `409 cart_taken`. Publishes `cart.taken`.
//! - `DELETE /held-carts/{id}`: drop it without resuming (`sale.void`, overridable with
//!   `context_ref` = the cart ID); audited as `held_cart.delete` with the cart. A taken cart →
//!   `409 cart_taken`.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sultan_core::access::Permission;
use sultan_core::ids::HeldCartId;
use sultan_core::money::Money;
use sultan_db::sales::{self, Actor, HeldCart, Written};
use utoipa::ToSchema;
use uuid::Uuid;

use super::sales::{Fail, run};
use super::{parse_id, parse_opt_id, parse_put_id};
use crate::access::Caller;
use crate::error::ApiError;
use crate::events::Event;
use crate::extract::{ApiJson, RegisterSession, UserSession};
use crate::overrides::{self, Action, OverrideHeader};
use crate::state::AppState;

/// Body of `PUT /held-carts/{id}`.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct HoldBody {
    /// The attached customer.
    #[serde(default)]
    pub customer_id: Option<String>,
    /// An optional note (at most 200 characters).
    #[serde(default)]
    pub note: Option<String>,
    /// The cart as the register keeps it.
    #[schema(value_type = Object)]
    pub payload: Value,
    /// The item count shown in the resume list.
    pub item_count: u32,
    /// The total shown in the resume list.
    pub total: i64,
}

/// A held cart.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct HeldCartDto {
    /// ID.
    pub id: String,
    /// The register that held it.
    pub register_id: String,
    /// The cashier who held it.
    pub user_id: String,
    /// The attached customer.
    pub customer_id: Option<String>,
    /// The note.
    pub note: Option<String>,
    /// The cart.
    #[schema(value_type = Object)]
    pub payload: Value,
    /// Item count.
    pub item_count: u32,
    /// Total.
    pub total: i64,
    /// Held at (unix ms).
    pub held_at: i64,
    /// Who resumed it.
    pub taken_by: Option<String>,
}

impl From<&HeldCart> for HeldCartDto {
    fn from(c: &HeldCart) -> Self {
        Self {
            id: c.id.to_string(),
            register_id: c.register.to_string(),
            user_id: c.user.to_string(),
            customer_id: c.customer.map(|c| c.to_string()),
            note: c.note.clone(),
            payload: c.payload.clone(),
            item_count: c.item_count,
            total: c.total.minor(),
            held_at: c.held_at.unix_millis(),
            taken_by: c.taken_by.map(|u| u.to_string()),
        }
    }
}

fn event(kind: &'static str, cart: &HeldCart) -> Event {
    Event::for_branch(
        kind,
        cart.branch,
        json!({ "held_cart_id": cart.id.to_string(), "register_id": cart.register.to_string() }),
    )
}

/// `404 not_found` unless the cart exists at the caller's branch.
async fn of_branch(
    state: &AppState,
    caller: &RegisterSession,
    id: HeldCartId,
) -> Result<(), ApiError> {
    sales::get_held_cart(&state.db, id)
        .await?
        .filter(|c| c.branch == caller.register.branch)
        .map(|_| ())
        .ok_or_else(|| ApiError::not_found("held cart"))
}

fn actor(caller: &RegisterSession) -> Actor {
    Actor {
        user: Some(caller.user.user.id),
        branch: Some(caller.register.branch),
        register: Some(caller.register.register),
    }
}

/// `PUT /api/v1/held-carts/{id}` (see the module docs).
///
/// # Errors
/// `403 register_session_required` / `permission_denied`, `409 cart_taken`, `422 invalid_field`.
#[utoipa::path(put, path = "/api/v1/held-carts/{id}", tag = "sales",
    security(("bearer" = [], "register_token" = [])),
    params(("id" = String, Path, description = "Client-generated UUIDv7")),
    request_body = HoldBody,
    responses((status = 201, body = HeldCartDto), (status = 200, body = HeldCartDto),
        (status = 403, body = crate::openapi::Problem), (status = 409, body = crate::openapi::Problem),
        (status = 422, body = crate::openapi::Problem)))]
pub async fn put(
    State(state): State<AppState>,
    caller: RegisterSession,
    Path(id): Path<String>,
    ApiJson(body): ApiJson<HoldBody>,
) -> Result<(StatusCode, Json<HeldCartDto>), ApiError> {
    let id: HeldCartId = parse_put_id(&id)?;
    Caller::load(&state.db, &caller.user)
        .await?
        .require(Permission::SaleCreate)?;
    if body.total < 0 {
        return Err(ApiError::invalid_field(
            "total",
            "`total` must not be negative",
        ));
    }
    let customer: Option<Uuid> = parse_opt_id(body.customer_id.as_deref(), "customer_id")?;
    let cart = HeldCart {
        id,
        branch: caller.register.branch,
        register: caller.register.register,
        user: caller.user.user.id,
        customer,
        note: body.note,
        payload: body.payload,
        item_count: body.item_count,
        total: Money::from_minor(body.total),
        held_at: state.now(),
        taken_by: None,
        taken_register: None,
        taken_at: None,
    };
    let (state_ref, cart_ref) = (&state, &cart);
    let (written, stored) = run("held cart put", || async move {
        let mut tx = state_ref.db.begin().await?;
        let written = sales::put_held_cart(&mut tx, cart_ref).await?;
        let stored = sales::get_held_cart(&mut tx, id)
            .await?
            .ok_or(sales::SaleDbError::NotFound("held cart"))?;
        tx.commit().await?;
        Ok::<_, Fail>((written, stored))
    })
    .await?;
    state.events.publish(event("cart.held", &stored));
    let status = if written == Written::Created {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    Ok((status, Json(HeldCartDto::from(&stored))))
}

/// `GET /api/v1/held-carts`: the waiting carts of the session's branch, newest first.
///
/// # Errors
/// `401`.
#[utoipa::path(get, path = "/api/v1/held-carts", tag = "sales",
    security(("bearer" = []), ("bearer" = [], "register_token" = [])),
    responses((status = 200, body = [HeldCartDto]), (status = 401, body = crate::openapi::Problem)))]
pub async fn list(
    State(state): State<AppState>,
    caller: UserSession,
) -> Result<Json<Vec<HeldCartDto>>, ApiError> {
    let carts = sales::list_held_carts(&state.db, caller.session.branch).await?;
    Ok(Json(carts.iter().map(HeldCartDto::from).collect()))
}

/// `POST /api/v1/held-carts/{id}/take` (see the module docs).
///
/// # Errors
/// `403`, `404 not_found`, `409 cart_taken`.
#[utoipa::path(post, path = "/api/v1/held-carts/{id}/take", tag = "sales",
    security(("bearer" = [], "register_token" = [])),
    params(("id" = String, Path, description = "Held cart ID"),
        ("Idempotency-Key" = Option<String>, Header, description = "Retry key (docs/plan.md §1)")),
    responses((status = 200, body = HeldCartDto), (status = 403, body = crate::openapi::Problem),
        (status = 404, body = crate::openapi::Problem), (status = 409, body = crate::openapi::Problem)))]
pub async fn take(
    State(state): State<AppState>,
    caller: RegisterSession,
    Path(id): Path<String>,
) -> Result<Json<HeldCartDto>, ApiError> {
    let id: HeldCartId = parse_id(&id, "id")?;
    Caller::load(&state.db, &caller.user)
        .await?
        .require(Permission::SaleCreate)?;
    of_branch(&state, &caller, id).await?;
    let (user, register, now) = (caller.user.user.id, caller.register.register, state.now());
    let state_ref = &state;
    let cart = run("held cart take", || async move {
        let mut tx = state_ref.db.begin().await?;
        let cart = sales::take_held_cart(&mut tx, id, user, register, now).await?;
        tx.commit().await?;
        Ok::<_, Fail>(cart)
    })
    .await?;
    state.events.publish(event("cart.taken", &cart));
    Ok(Json(HeldCartDto::from(&cart)))
}

/// `DELETE /api/v1/held-carts/{id}` (see the module docs).
///
/// # Errors
/// `403` permission codes, `404 not_found`, `409 cart_taken`.
#[utoipa::path(delete, path = "/api/v1/held-carts/{id}", tag = "sales",
    security(("bearer" = [], "register_token" = [])),
    params(("id" = String, Path, description = "Held cart ID"),
        ("X-Override" = Option<String>, Header, description = "Override token for `sale.void`")),
    responses((status = 200, body = HeldCartDto), (status = 403, body = crate::openapi::Problem),
        (status = 404, body = crate::openapi::Problem), (status = 409, body = crate::openapi::Problem)))]
pub async fn delete(
    State(state): State<AppState>,
    caller: RegisterSession,
    header: OverrideHeader,
    Path(id): Path<String>,
) -> Result<Json<HeldCartDto>, ApiError> {
    let id: HeldCartId = parse_id(&id, "id")?;
    let access = Caller::load(&state.db, &caller.user).await?;
    of_branch(&state, &caller, id).await?;
    let context = id.to_string();
    let actor = actor(&caller);
    let now = state.now();
    let (state_ref, caller_ref, access_ref, header_ref, context_ref, actor_ref) =
        (&state, &caller, &access, &header, &context, &actor);
    let cart = run("held cart delete", || async move {
        let mut tx = state_ref.db.begin().await?;
        overrides::authorize(
            state_ref,
            &mut tx,
            caller_ref,
            &access_ref.permissions,
            Action {
                permission: Permission::SaleVoid,
                value: None,
                context_ref,
            },
            header_ref,
        )
        .await?;
        let cart = sales::delete_held_cart(&mut tx, id, actor_ref, now).await?;
        tx.commit().await?;
        Ok::<_, Fail>(cart)
    })
    .await?;
    state.events.publish(event("cart.taken", &cart));
    Ok(Json(HeldCartDto::from(&cart)))
}
