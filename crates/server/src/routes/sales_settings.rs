//! The sales settings (spec 2.6 §8, §9; phase S6, deferred from S3).
//!
//! - `GET /settings/sales`: `{return_days, receipt_widths: [{branch_id, receipt_width}]}`, every
//!   branch with its width (32 when none is stored). Any user session or the device token.
//! - `PUT /settings/sales {return_days, receipt_widths?}`: replaces `return_days` (0–3650, `0` =
//!   no return window) and sets the width of each listed branch (32, 42 or 48); branches that
//!   are not listed keep theirs. HQ only (`403 read_only_replica`) and `tender.manage` from any
//!   role assignment (the HQ sales permission, like tender types). Audit and outbox
//!   `sales_settings.put` in the same transaction; an unchanged replay writes nothing.
//!
//! The return window of `PUT /returns/{id}` and the default width of the receipts
//! (`GET /sales/{id}/receipt`, `GET /returns/{id}/receipt`) read these.

use axum::Json;
use axum::extract::State;
use serde::{Deserialize, Serialize};
use sultan_core::access::Permission;
use sultan_core::ids::BranchId;
use sultan_db::org;
use sultan_db::sales::{self, SalesSettings};
use utoipa::ToSchema;

use super::catalog::actor;
use super::parse_id;
use super::sales::{Fail, run};
use crate::access::Caller;
use crate::error::ApiError;
use crate::extract::{ApiJson, RequireHq, UserSession};
use crate::state::AppState;

/// The receipt width of one branch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct BranchWidthDto {
    /// The branch.
    pub branch_id: String,
    /// 32, 42 or 48 columns.
    pub receipt_width: u16,
}

/// The sales settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct SalesSettingsDto {
    /// The return window in days (default 30; `0` = no limit).
    pub return_days: u32,
    /// The receipt width of each branch (`PUT`: only the listed branches change).
    #[serde(default)]
    pub receipt_widths: Vec<BranchWidthDto>,
}

async fn load(state: &AppState) -> Result<SalesSettingsDto, ApiError> {
    let stored = sales::get_sales_settings(&state.db).await?;
    let branches = org::list_branches(&state.db).await?;
    Ok(SalesSettingsDto {
        return_days: stored.return_days,
        receipt_widths: branches
            .iter()
            .map(|b| BranchWidthDto {
                branch_id: b.id.to_string(),
                receipt_width: stored
                    .receipt_widths
                    .iter()
                    .find(|(id, _)| *id == b.id)
                    .map_or(sales::DEFAULT_RECEIPT_WIDTH, |(_, w)| *w),
            })
            .collect(),
    })
}

/// `GET /api/v1/settings/sales`.
///
/// # Errors
/// `401`.
#[utoipa::path(get, path = "/api/v1/settings/sales", tag = "sales",
    security(("bearer" = []), ("bearer" = [], "register_token" = [])),
    responses((status = 200, body = SalesSettingsDto), (status = 401, body = crate::openapi::Problem)))]
pub async fn get(
    State(state): State<AppState>,
    _session: UserSession,
) -> Result<Json<SalesSettingsDto>, ApiError> {
    Ok(Json(load(&state).await?))
}

/// `PUT /api/v1/settings/sales` (see the module docs).
///
/// # Errors
/// `400 invalid_body` (a `branch_id` that is not a UUID), `403 read_only_replica` /
/// `permission_denied` (`tender.manage`), `404 not_found` (a branch), `422 invalid_field`
/// (`return_days`, `receipt_width`).
#[utoipa::path(put, path = "/api/v1/settings/sales", tag = "sales",
    security(("bearer" = [])), request_body = SalesSettingsDto,
    responses((status = 200, body = SalesSettingsDto), (status = 403, body = crate::openapi::Problem),
        (status = 404, body = crate::openapi::Problem), (status = 422, body = crate::openapi::Problem)))]
pub async fn put(
    State(state): State<AppState>,
    _hq: RequireHq,
    session: UserSession,
    ApiJson(body): ApiJson<SalesSettingsDto>,
) -> Result<Json<SalesSettingsDto>, ApiError> {
    Caller::load(&state.db, &session)
        .await?
        .require_anywhere(Permission::TenderManage)?;
    let mut receipt_widths = Vec::with_capacity(body.receipt_widths.len());
    for w in &body.receipt_widths {
        let branch: BranchId = parse_id(&w.branch_id, "branch_id")?;
        receipt_widths.push((branch, w.receipt_width));
    }
    let settings = SalesSettings {
        return_days: body.return_days,
        receipt_widths,
    };
    let now = state.now();
    let (state_ref, actor, settings) = (&state, &actor(&session), &settings);
    run("sales settings put", || async move {
        let mut tx = state_ref.db.begin().await?;
        sales::put_sales_settings(&mut tx, settings, actor, now).await?;
        tx.commit().await?;
        Ok::<_, Fail>(())
    })
    .await?;
    Ok(Json(load(&state).await?))
}
