//! The fake's sales (spec 2.6, phase S3): tender types, posted sales, post-voids, till voids,
//! receipts, returns and exchanges, held carts and the purchase history, close enough to the
//! server for the register and back-office UI tests (S4–S6).
//!
//! **Rules kept:** a sale needs the caller's open session (`409 no_open_session`); the cart is
//! priced again with the fake's quote and `core::sale::compute_totals`, and posted totals that
//! differ answer `409 cart_stale` with `quote.totals`; tenders go through `core::sale::Payment`
//! (references, only cash above the rest, the customer balance, saved change with the stored
//! limits); a replay of the same body answers the stored sale, another body `409 conflict`.
//! Line and cart discounts above the caller's limit, a post-void, a cart void, dropping a held
//! cart, a reprint of an older receipt and a return without a receipt need a manager: a register
//! user marked [`super::FakeUser::with_reports`] (Budi), or an `X-Override` token from
//! [`crate::Service::request_override`] for that action and the document's ID. Returns by
//! receipt use `core::sale::line_refund` (`return_qty_exceeded`), a sale with returns cannot be
//! voided (`has_returns`), a held cart is taken once (`cart_taken`). Cash taken and paid back
//! moves the session's expected cash.
//!
//! Receipts are built with `core::sale::build_receipt` like the server's (32 columns, labels in
//! the asked language, the catalog's product and variant names, the tender type names).
//!
//! Refunds (post-void, return) go to cash or to the customer balance (the balance part of the sale
//! goes back first on a post-void); returns and post-voids take only sales of the fake's branch
//! (`invalid_field`, `sale`); a return by receipt older than 30 days answers `403 return_window`
//! unless a token approves `refund.with_receipt` for the return. `POST /drawer/open` needs the
//! caller's open session and, for a cashier, a `drawer.open` token for that session. The purchase
//! history pages by 10.
//!
//! **Simplified:** no stock, no cash rounding, the receipt's branch block and footer are fixed demo
//! text and it prints no tier name.

use std::collections::HashMap;

use serde_json::json;
use sultan_core::catalog::Qty;
use sultan_core::customer::BalanceSettings as CoreLimits;
use sultan_core::ids::TenderTypeId;
use sultan_core::money::Money;
use sultan_core::pricing::CashRounding;
use sultan_core::sale::{
    BalanceContext, Discount as CoreDiscount, Payment, ReturnedSoFar, SoldLine,
    TenderType as CoreTender, TotalsLine, compute_totals, line_refund,
};
use sultan_core::tax::TaxSettings;

use super::{BRANCH_ID, FakeService, REGISTER_ID, State, api, ready};
use crate::sales::{
    CartVoid, CartVoidRequest, Discount, DocQuery, HeldCart, HistoryItem, HistoryPage, HoldRequest,
    Receipt, RefundTender, ReturnLine, ReturnPage, ReturnRequest, ReturnSummary, Sale, SaleLine,
    SalePage, SaleRequest, SaleReturn, SaleSummary, SaleTender, SaleTotals, SaleVoid,
    SalesSettings, TenderType, TenderTypeBody, VoidRequest,
};
use crate::{ClientError, Problem, QuoteLine, QuoteRequest};

/// The seeded tender types (as on the server, with fake IDs).
pub const TENDER_CASH: &str = "tender-cash";
/// Kartu debit (reference required).
pub const TENDER_CARD: &str = "tender-card";
/// QRIS (reference required).
pub const TENDER_QRIS: &str = "tender-qris";
/// Saldo pelanggan.
pub const TENDER_BALANCE: &str = "tender-balance";

/// The fake's sales state.
#[allow(clippy::struct_field_names)] // `sales` next to returns and held carts reads best
pub(super) struct Sales {
    tenders: Vec<TenderType>,
    sales: Vec<(SaleRequest, Sale)>,
    returns: Vec<(ReturnRequest, SaleReturn)>,
    held: Vec<HeldCart>,
    voids: Vec<CartVoid>,
    drawer: Vec<crate::DrawerOpen>,
    keyed: HashMap<String, Sale>,
    /// A line void needs a manager too (as a cashier without `sale.void_line` would).
    pub(super) line_void_override: bool,
    /// The sales settings (phase S6).
    settings: SalesSettings,
}

impl Default for Sales {
    fn default() -> Self {
        let tender = |id: &str, name: &str, kind: &str, reference: bool, sort: u32| TenderType {
            id: id.to_owned(),
            name: name.to_owned(),
            kind: kind.to_owned(),
            needs_reference: reference,
            opens_drawer: kind == "cash",
            active: true,
            sort,
            archived: false,
        };
        Self {
            tenders: vec![
                tender(TENDER_CASH, "Tunai", "cash", false, 1),
                tender(TENDER_CARD, "Kartu debit", "card", true, 2),
                tender(TENDER_QRIS, "QRIS", "qris", true, 3),
                tender(
                    TENDER_BALANCE,
                    "Saldo pelanggan",
                    "customer_balance",
                    false,
                    4,
                ),
            ],
            sales: Vec::new(),
            returns: Vec::new(),
            held: Vec::new(),
            voids: Vec::new(),
            drawer: Vec::new(),
            keyed: HashMap::new(),
            line_void_override: false,
            settings: SalesSettings {
                return_days: 30,
                receipt_widths: [BRANCH_ID, super::OTHER_BRANCH_ID]
                    .into_iter()
                    .map(|b| crate::BranchWidth {
                        branch_id: b.to_owned(),
                        receipt_width: 32,
                    })
                    .collect(),
            },
        }
    }
}

impl Sales {
    /// No-sale drawer opens of a cashier session (spec 2.7 K22).
    pub(super) fn drawer_opens(&self, session: &str) -> u32 {
        let n = self
            .drawer
            .iter()
            .filter(|d| d.cashier_session_id == session)
            .count();
        u32::try_from(n).unwrap_or(u32::MAX)
    }
}

fn rule(error: &sultan_core::sale::SaleError) -> ClientError {
    let mut p = Problem::new(422, error.code(), &error.to_string());
    if let Some(field) = error.field() {
        p = p.with("field", field);
    }
    if matches!(
        error.code(),
        "cart_stale" | "cart_taken" | "has_returns" | "conflict"
    ) {
        p.status = 409;
    }
    p.into()
}

/// A purchase history page.
const HISTORY_PAGE: usize = 10;

fn invalid(field: &str) -> ClientError {
    Problem::new(422, "invalid_field", field)
        .with("field", field)
        .into()
}

fn core_discount(d: Option<&Discount>) -> Option<CoreDiscount> {
    d.and_then(|d| match (d.pct_bp, d.amount) {
        (Some(bp), _) => Some(CoreDiscount::Pct(bp)),
        (None, Some(a)) => Some(CoreDiscount::Amount(Money::from_minor(a))),
        (None, None) => None,
    })
}

impl State {
    /// The caller's open session, or `409 no_open_session`.
    fn own_session(&self, token: &str) -> Result<(super::FakeUser, String), ClientError> {
        let user = self.caller(token)?;
        match &self.session {
            Some(s) if s.user_id == user.id => Ok((user, s.id.clone())),
            _ => Err(api(409, "no_open_session")),
        }
    }

    /// `Ok(approver)` when `user` is a manager or `token` approves `action` on `context`.
    fn manager_or_override(
        &mut self,
        user: &super::FakeUser,
        action: &str,
        value: Option<i64>,
        context: &str,
        token: Option<&str>,
    ) -> Result<Option<String>, ClientError> {
        if user.reports {
            return Ok(None);
        }
        let Some(token) = token else {
            return Err(Problem::new(403, "needs_override", "needs override")
                .with("permission", action)
                .into());
        };
        for t in token.split(',').map(str::trim) {
            if let Some((a, v, c, approver)) = self.overrides.get(t).cloned()
                && a == action
                && c == context
                && (v == value || value.is_none())
            {
                self.overrides.remove(t);
                return Ok(Some(approver));
            }
        }
        Err(api(403, "invalid_override"))
    }

    /// Every check above the caller's limit takes one matching token (the server's
    /// `authorize_all`): none for the first unmatched check → `needs_override` (with `permission`
    /// and `limit`), or `invalid_override` when tokens were sent and none matched yet.
    fn authorize_all(
        &mut self,
        user: &super::FakeUser,
        checks: &[(&str, i64)],
        context: &str,
        header: Option<&str>,
    ) -> Result<(), ClientError> {
        if user.reports {
            return Ok(());
        }
        let mut left: Vec<String> = header
            .map(|h| {
                h.split(',')
                    .map(str::trim)
                    .filter(|t| !t.is_empty())
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        let sent = !left.is_empty();
        let mut used_any = false;
        let mut matched = Vec::new();
        for (action, value) in checks {
            if *value <= user.discount_limit {
                continue;
            }
            let found = left.iter().position(|t| {
                self.overrides
                    .get(t)
                    .is_some_and(|(a, v, c, _)| a == action && c == context && *v == Some(*value))
            });
            match found {
                Some(i) => {
                    matched.push(left.remove(i));
                    used_any = true;
                }
                None if sent && !used_any => return Err(api(403, "invalid_override")),
                None => {
                    return Err(Problem::new(403, "needs_override", "needs override")
                        .with("permission", *action)
                        .with("limit", user.discount_limit)
                        .into());
                }
            }
        }
        // Only a sale that goes through uses its tokens up (like the server's transaction).
        for t in matched {
            self.overrides.remove(&t);
        }
        Ok(())
    }

    /// The saved receipt width of a branch (32 columns when none is saved).
    fn receipt_width(&self, branch_id: &str) -> sultan_core::sale::ReceiptWidth {
        use sultan_core::sale::ReceiptWidth as W;
        self.sales
            .settings
            .receipt_widths
            .iter()
            .find(|w| w.branch_id == branch_id)
            .and_then(|w| W::from_cols(usize::from(w.receipt_width)))
            .unwrap_or_default()
    }

    /// Whether a sale posted at `at` is outside the return window (core's check, §19 item 13).
    fn outside_window(&self, at: i64) -> bool {
        use sultan_core::time::Timestamp;
        sultan_core::sale::check_return_window(
            Timestamp::from_unix_millis(at),
            Timestamp::from_unix_millis(self.now),
            sultan_core::doc_number::BranchTimeZone::default(),
            self.sales.settings.return_days,
        )
        .is_err()
    }

    /// A return outside the window: `403 return_window` unless a token approves
    /// `refund.with_receipt` for the return (§19 item 39).
    fn window_override(&mut self, return_id: &str, token: Option<&str>) -> Result<(), ClientError> {
        for t in token.into_iter().flat_map(|t| t.split(',')).map(str::trim) {
            if let Some((a, _, c, _)) = self.overrides.get(t).cloned()
                && a == "refund.with_receipt"
                && c == return_id
            {
                self.overrides.remove(t);
                return Ok(());
            }
        }
        Err(
            Problem::new(403, "return_window", "outside the return window")
                .with("days", self.sales.settings.return_days)
                .with("permission", "refund.with_receipt")
                .into(),
        )
    }

    fn drawer(
        &mut self,
        token: &str,
        body: &crate::DrawerRequest,
        override_token: Option<&str>,
    ) -> Result<crate::DrawerOpen, ClientError> {
        let (user, session) = self.own_session(token)?;
        let approver =
            self.manager_or_override(&user, "drawer.open", None, &session, override_token)?;
        let open = crate::DrawerOpen {
            cashier_session_id: session,
            reason: body
                .reason
                .as_deref()
                .map(str::trim)
                .filter(|r| !r.is_empty())
                .map(str::to_owned),
            approved_by: approver,
            opened_at: self.now,
        };
        self.sales.drawer.push(open.clone());
        Ok(open)
    }

    fn tender(&self, id: &str) -> Result<&TenderType, ClientError> {
        self.sales
            .tenders
            .iter()
            .find(|t| t.id == id)
            .ok_or_else(|| {
                Problem::new(422, "invalid_reference", "unknown tender type")
                    .with("field", "tenders.tender_type_id")
                    .into()
            })
    }

    fn number(&self, kind: &str) -> String {
        let n = self.sales.sales.len() + self.sales.returns.len() + 1;
        let day = sultan_core::doc_number::LocalDay::of(
            sultan_core::time::Timestamp::from_unix_millis(self.now),
            sultan_core::doc_number::BranchTimeZone::default(),
        );
        format!("JKT01-R1-{kind}{}-{n:04}", day.yymmdd())
    }

    /// Prices, checks and pays a sale (see the module docs); `exchange` pays it first.
    #[allow(clippy::too_many_lines)] // the posting rules in spec order
    fn build_sale(
        &mut self,
        token: &str,
        id: &str,
        body: &SaleRequest,
        exchange: Option<i64>,
        override_token: Option<&str>,
    ) -> Result<(Sale, i64), ClientError> {
        let (user, session) = self.own_session(token)?;
        if body.lines.is_empty() {
            return Err(Problem::new(422, "invalid_field", "no lines")
                .with("field", "lines")
                .into());
        }
        let quote = self.quote(
            token,
            &QuoteRequest {
                customer_id: body.customer_id.clone(),
                lines: body
                    .lines
                    .iter()
                    .map(|l| QuoteLine {
                        typed_price: l.typed_price,
                        ..QuoteLine::new(&l.variant_id, &l.unit_id, l.qty_thousandths)
                    })
                    .collect(),
            },
        )?;
        let mut totals_lines = Vec::new();
        for (q, l) in quote.lines.iter().zip(&body.lines) {
            if let Some(e) = &q.error {
                return Err(Problem::new(422, &e.code, &e.message).into());
            }
            totals_lines.push(TotalsLine {
                gross: Money::from_minor(q.amount.unwrap_or_default()),
                line_discount: core_discount(l.discount.as_ref()),
                cart_discountable: true,
                tax_rate_bp: q.tax.as_ref().map_or(0, |t| t.rate_bp),
            });
        }
        let tax = TaxSettings {
            inclusive: quote.tax_inclusive,
            registered: quote.tax_registered,
        };
        let totals = compute_totals(
            &totals_lines,
            core_discount(body.cart_discount.as_ref()),
            tax,
        )
        .map_err(|e| rule(&e))?;
        let fresh = SaleTotals {
            subtotal: totals.subtotal.minor(),
            line_discount: totals.line_discount.minor(),
            cart_discount: totals.cart_discount.minor(),
            tax: totals.tax.minor(),
            total: totals.total.minor(),
        };
        if fresh != body.totals {
            return Err(Problem::new(409, "cart_stale", "stale")
                .with("quote", json!({ "totals": fresh }))
                .into());
        }
        // Every limited check of the sale in the server's order (lines, then the cart
        // discount), each against the caller's limit; one token per approval (§19 item 36).
        let mut checks: Vec<(&str, i64)> = Vec::new();
        for (q, t) in quote.lines.iter().zip(&totals.lines) {
            if q.mode == "overridden"
                && let (Some(resolved), Some(price)) = (q.resolved_price, q.unit_price)
            {
                let drop = sultan_core::sale::discount_bp(
                    Money::from_minor(resolved),
                    Money::from_minor((resolved - price).max(0)),
                );
                checks.push(("price.override", i64::from(drop)));
            }
            if t.line_discount > Money::ZERO {
                checks.push(("sale.discount", i64::from(t.line_discount_bp())));
            }
        }
        if totals.cart_discount > Money::ZERO {
            checks.push(("sale.cart_discount", i64::from(totals.cart_discount_bp())));
        }
        self.authorize_all(&user, &checks, id, override_token)?;
        let balance = body
            .customer_id
            .as_deref()
            .and_then(|c| self.balance_of(c))
            .map(|b| BalanceContext {
                balance: Money::from_minor(b),
            });
        let rounding: CashRounding = quote.cash_rounding.parse().unwrap_or_default();
        let core_ids: HashMap<String, TenderTypeId> = self
            .sales
            .tenders
            .iter()
            .map(|t| (t.id.clone(), TenderTypeId::generate()))
            .collect();
        let mut payment = Payment::new(totals.total, rounding).map_err(|e| rule(&e))?;
        let mut exchanged = 0;
        if let Some(refund) = exchange {
            exchanged = refund.min(totals.total.minor()).max(0);
            if exchanged > 0 {
                payment
                    .add_exchange(Money::from_minor(exchanged))
                    .map_err(|e| rule(&e))?;
            }
        }
        for t in &body.tenders {
            let tt = self.tender(&t.tender_type_id)?;
            let core = CoreTender {
                id: core_ids[&tt.id],
                name: tt.name.clone(),
                kind: tt.kind.parse().map_err(|_| api(422, "invalid_field"))?,
                needs_reference: tt.needs_reference,
                opens_drawer: tt.opens_drawer,
                active: tt.active && !tt.archived,
            };
            payment
                .add(
                    &core,
                    Money::from_minor(t.amount),
                    t.reference.as_deref(),
                    balance,
                )
                .map_err(|e| rule(&e))?;
        }
        if let Some(saved) = body.save_change.filter(|s| *s != 0) {
            let limits = self.balance_limits();
            let limits = CoreLimits {
                max_saved_per_sale: Money::from_minor(limits.max_saved_per_sale),
                max_balance: Money::from_minor(limits.max_balance),
            };
            payment
                .save_change(limits, balance, Money::from_minor(saved))
                .map_err(|e| rule(&e))?;
        }
        let paid = payment.finish().map_err(|e| rule(&e))?;
        let ids: HashMap<TenderTypeId, String> =
            core_ids.into_iter().map(|(k, v)| (v, k)).collect();
        let tenders: Vec<SaleTender> = paid
            .tenders
            .iter()
            .map(|t| SaleTender {
                tender_type_id: t.tender_type.and_then(|id| ids.get(&id).cloned()),
                kind: t.kind.as_str().to_owned(),
                amount: t.amount.minor(),
                cash_received: t.cash_received.map(Money::minor),
                change_given: t.change_given.minor(),
                change_saved: t.change_saved.minor(),
                reference: t.reference.clone(),
            })
            .collect();
        let lines = body
            .lines
            .iter()
            .zip(&quote.lines)
            .zip(&totals.lines)
            .enumerate()
            .map(|(i, ((l, q), t))| SaleLine {
                line_no: u32::try_from(i + 1).unwrap_or(u32::MAX),
                variant_id: l.variant_id.clone(),
                unit_id: l.unit_id.clone(),
                name: self
                    .line_names(&l.variant_id, &l.unit_id)
                    .map_or_else(|| l.variant_id.clone(), |(name, _)| name),
                qty_thousandths: l.qty_thousandths,
                unit_price: q.unit_price.unwrap_or_default(),
                price_mode: q.mode.clone(),
                resolved_price: q.resolved_price,
                gross: t.gross.minor(),
                line_discount: t.line_discount.minor(),
                cart_discount_share: t.cart_discount_share.minor(),
                amount: t.amount.minor(),
                tax_rate_bp: t.tax_rate_bp,
                tax: t.tax.minor(),
                total: t.total.minor(),
                ..SaleLine::default()
            })
            .collect::<Vec<_>>();
        let sum = |f: fn(&SaleTender) -> i64| tenders.iter().map(f).sum::<i64>();
        let summary = SaleSummary {
            id: id.to_owned(),
            number: self.number(""),
            status: "posted".to_owned(),
            branch_id: BRANCH_ID.to_owned(),
            register_id: Some(REGISTER_ID.to_owned()),
            cashier_session_id: Some(session),
            user_id: Some(user.id),
            customer_id: body.customer_id.clone(),
            customer_name: self.customer_name(body.customer_id.as_deref()),
            customer_code: self.customer_code(body.customer_id.as_deref()),
            cashier_name: Some(user.name.clone()),
            register_code: Some("R1".to_owned()),
            tax_invoice: body.tax_invoice,
            tax_inclusive: tax.inclusive,
            subtotal: fresh.subtotal,
            line_discount: fresh.line_discount,
            cart_discount: fresh.cart_discount,
            tax: fresh.tax,
            total: fresh.total,
            cash_rounding: paid.cash_rounding.minor(),
            paid: fresh.total + paid.cash_rounding.minor(),
            change_given: sum(|t| t.change_given),
            change_saved: sum(|t| t.change_saved),
            line_count: u32::try_from(lines.len()).unwrap_or(u32::MAX),
            posted_at: self.now,
            ..SaleSummary::default()
        };
        if let Some(c) = &body.customer_id {
            for e in paid.balance_entries {
                self.entry(c, e.kind().as_str(), e.amount().minor());
            }
        }
        self.cash_sales += tenders
            .iter()
            .filter(|t| t.kind == "cash")
            .map(|t| t.amount + t.change_saved)
            .sum::<i64>();
        Ok((
            Sale {
                summary,
                lines,
                tenders,
                void: None,
            },
            exchanged,
        ))
    }

    fn post_sale(
        &mut self,
        token: &str,
        id: &str,
        body: &SaleRequest,
        override_token: Option<&str>,
    ) -> Result<Sale, ClientError> {
        if let Some((request, sale)) = self.sales.sales.iter().find(|(_, s)| s.summary.id == id) {
            return if request == body {
                Ok(sale.clone())
            } else {
                Err(api(409, "conflict"))
            };
        }
        let (sale, _) = self.build_sale(token, id, body, None, override_token)?;
        self.sales.sales.push((body.clone(), sale.clone()));
        Ok(sale)
    }

    fn sale_index(&self, id: &str) -> Result<usize, ClientError> {
        self.sales
            .sales
            .iter()
            .position(|(_, s)| s.summary.id == id || s.summary.number == id)
            .ok_or_else(|| api(404, "not_found"))
    }

    fn list_sales(&self, query: &DocQuery) -> SalePage {
        let all: Vec<SaleSummary> = self
            .sales
            .sales
            .iter()
            .rev()
            .map(|(_, s)| s.summary.clone())
            .filter(|s| {
                query
                    .cashier_session_id
                    .as_ref()
                    .is_none_or(|q| s.cashier_session_id.as_ref() == Some(q))
                    && query
                        .customer_id
                        .as_ref()
                        .is_none_or(|q| s.customer_id.as_ref() == Some(q))
                    && query.number.as_ref().is_none_or(|q| &s.number == q)
                    && query.status.as_ref().is_none_or(|q| &s.status == q)
                    && query.branch_id.as_ref().is_none_or(|q| &s.branch_id == q)
                    && query
                        .register_id
                        .as_ref()
                        .is_none_or(|q| s.register_id.as_ref() == Some(q))
                    && query.from.is_none_or(|f| s.posted_at >= f)
                    && query.until.is_none_or(|u| s.posted_at < u)
            })
            .collect();
        let (items, next_before_id) = page_of(all, query, |s| &s.id);
        SalePage {
            items,
            next_before_id,
        }
    }

    /// The name of customer `id`, as the server joins it into lists.
    fn customer_name(&self, id: Option<&str>) -> Option<String> {
        id.and_then(|c| self.customer_record(c))
            .map(|c| c.name.clone())
    }

    /// The member code of customer `id`.
    fn customer_code(&self, id: Option<&str>) -> Option<String> {
        id.and_then(|c| self.customer_record(c))
            .map(|c| c.code.clone())
    }

    #[allow(clippy::too_many_lines)] // every block of `ReturnReceiptInput`, as the server builds it
    fn return_receipt(
        &mut self,
        token: &str,
        id: &str,
        copy: bool,
        lang: &str,
        override_token: Option<&str>,
    ) -> Result<Receipt, ClientError> {
        use sultan_core::customer::{Phone, ReceiptCustomer};
        use sultan_core::sale::{
            Label, ReceiptHeader, ReturnReceiptInput, ReturnReceiptLine, ReturnReceiptTender,
            build_return_receipt,
        };
        self.any_caller(token)?;
        let ret = self
            .sales
            .returns
            .iter()
            .find(|(_, r)| r.summary.id == id)
            .map(|(_, r)| r.clone())
            .ok_or_else(|| api(404, "not_found"))?;
        if copy {
            let user = self.caller(token).ok();
            let last = user.as_ref().is_some_and(|u| {
                self.session
                    .as_ref()
                    .filter(|s| s.user_id == u.id)
                    .and_then(|s| {
                        self.sales
                            .returns
                            .iter()
                            .rev()
                            .find(|(_, x)| x.summary.cashier_session_id == s.id)
                    })
                    .is_some_and(|(_, x)| x.summary.id == id)
            });
            match user {
                Some(u) if !last => {
                    self.manager_or_override(&u, "sale.reprint", None, id, override_token)?;
                }
                _ => {}
            }
        }
        let s = &ret.summary;
        let units: Vec<String> = ret
            .lines
            .iter()
            .map(|l| {
                self.line_names(&l.variant_id, &l.unit_id)
                    .map(|(_, unit)| unit)
                    .unwrap_or_default()
            })
            .collect();
        let lines: Vec<ReturnReceiptLine<'_>> = ret
            .lines
            .iter()
            .zip(&units)
            .map(|(l, unit)| ReturnReceiptLine {
                name: &l.name,
                qty: Qty::from_thousandths(l.qty_thousandths),
                unit,
                refund: Money::from_minor(l.refund),
                write_off: l.disposition == "write_off",
            })
            .collect();
        let exchange = Label::Exchange.text(lang).to_owned();
        let tenders: Vec<ReturnReceiptTender<'_>> = ret
            .tenders
            .iter()
            .map(|t| ReturnReceiptTender {
                name: t
                    .tender_type_id
                    .as_deref()
                    .and_then(|id| self.tender(id).ok())
                    .map_or(exchange.as_str(), |tt| tt.name.as_str()),
                amount: Money::from_minor(t.amount),
            })
            .collect();
        let customer = s
            .customer_id
            .as_deref()
            .and_then(|c| self.customer_record(c));
        let phone = customer
            .and_then(|c| c.phone.as_deref())
            .and_then(|p| Phone::parse(p).ok());
        let number_of = |id: Option<&String>| {
            id.and_then(|id| self.sale_index(id).ok())
                .map(|i| self.sales.sales[i].1.summary.number.clone())
        };
        let sale_number = number_of(s.sale_id.as_ref());
        let exchange_number = number_of(s.exchange_sale_id.as_ref());
        let cashier = self
            .user(&s.user_id)
            .map(|u| u.name.clone())
            .unwrap_or_default();
        let footer = ["Terima kasih".to_owned()];
        let input = ReturnReceiptInput {
            width: self.receipt_width(&s.branch_id),
            header: ReceiptHeader {
                lines: &[],
                branch_name: "Toko Jakarta",
                address: "Jl. Contoh No. 1, Jakarta",
                tax_id: None,
            },
            number: &s.number,
            posted_at: sultan_core::time::Timestamp::from_unix_millis(s.posted_at),
            tz: sultan_core::doc_number::BranchTimeZone::default(),
            register: "R1",
            cashier: &cashier,
            customer: customer.map(|c| ReceiptCustomer {
                name: &c.name,
                company_name: c.company_name.as_deref(),
                phone: phone.as_ref(),
                tier_name: None,
            }),
            sale_number: sale_number.as_deref(),
            lines: &lines,
            cash_rounding: Money::from_minor(s.cash_rounding),
            refund: Money::from_minor(s.refund),
            tenders: &tenders,
            exchange_number: exchange_number.as_deref(),
            footer: &footer,
            copy,
        };
        let built = build_return_receipt(&input, &|l| l.text(lang).to_owned());
        Ok(Receipt {
            return_id: s.id.clone(),
            number: s.number.clone(),
            copy,
            text: built.text(),
            lines: built.lines,
            barcode: built.barcode,
            ..Receipt::default()
        })
    }

    fn receipt(
        &mut self,
        token: &str,
        id: &str,
        copy: bool,
        lang: &str,
        override_token: Option<&str>,
    ) -> Result<Receipt, ClientError> {
        let sale = self.sales.sales[self.sale_index(id)?].1.clone();
        if copy && self.caller(token).is_err() {
            // The back office reprints with `sale.reprint` (its manager users).
            self.any_caller(token)?;
        } else if copy {
            let user = self.caller(token)?;
            let last = self
                .session
                .as_ref()
                .filter(|s| s.user_id == user.id)
                .and_then(|s| {
                    self.sales
                        .sales
                        .iter()
                        .rev()
                        .find(|(_, x)| x.summary.cashier_session_id.as_ref() == Some(&s.id))
                })
                .is_some_and(|(_, x)| x.summary.id == sale.summary.id);
            if !last {
                self.manager_or_override(&user, "sale.reprint", None, id, override_token)?;
            }
        }
        self.any_caller(token)?;
        let voided = sale.summary.status == "voided";
        let built = self.build_receipt(&sale, copy, voided, lang);
        let (text, lines) = (built.text(), built.lines);
        Ok(Receipt {
            sale_id: sale.summary.id.clone(),
            number: sale.summary.number.clone(),
            copy,
            voided,
            lines,
            text,
            barcode: built.barcode,
            return_id: String::new(),
        })
    }

    /// The receipt text of `sale` with `core::sale::build_receipt`, as the server builds it
    /// (32 columns, labels in `lang`), from what the fake knows: the branch, the register, the
    /// cashier, the customer, the catalog's names and the tender types.
    #[allow(clippy::too_many_lines)] // every block of `ReceiptInput`, in the server's order
    fn build_receipt(
        &self,
        sale: &Sale,
        copy: bool,
        voided: bool,
        lang: &str,
    ) -> sultan_core::sale::Receipt {
        use std::collections::BTreeMap;
        use sultan_core::customer::{Phone, ReceiptCustomer};
        use sultan_core::sale::{
            Label, ReceiptCustomerBlock, ReceiptHeader, ReceiptInput, ReceiptLine, ReceiptSaved,
            ReceiptTender, ReceiptTotals, TaxInvoiceBlock, TaxLine, build_receipt,
        };
        let s = &sale.summary;
        let cashier = s
            .user_id
            .as_deref()
            .and_then(|u| self.user(u))
            .map(|u| u.name.clone())
            .unwrap_or_default();
        let units: Vec<String> = sale
            .lines
            .iter()
            .map(|l| {
                self.line_names(&l.variant_id, &l.unit_id)
                    .map(|(_, unit)| unit)
                    .unwrap_or_default()
            })
            .collect();
        let lines: Vec<ReceiptLine<'_>> = sale
            .lines
            .iter()
            .zip(&units)
            .map(|(l, unit)| ReceiptLine {
                name: &l.name,
                qty: (l.qty_thousandths != 0).then(|| Qty::from_thousandths(l.qty_thousandths)),
                unit,
                unit_price: Money::from_minor(l.unit_price),
                gross: Money::from_minor(l.gross),
                discount: Money::from_minor(l.line_discount),
            })
            .collect();
        let mut by_rate: BTreeMap<u16, (Money, Money)> = BTreeMap::new();
        for l in sale.lines.iter().filter(|l| l.tax_rate_bp > 0) {
            let e = by_rate.entry(l.tax_rate_bp).or_default();
            e.0 = e.0.saturating_add(Money::from_minor(l.total - l.tax));
            e.1 = e.1.saturating_add(Money::from_minor(l.tax));
        }
        let exchange = Label::Exchange.text(lang).to_owned();
        let tenders: Vec<ReceiptTender<'_>> = sale
            .tenders
            .iter()
            .map(|t| ReceiptTender {
                name: t
                    .tender_type_id
                    .as_deref()
                    .and_then(|id| self.tender(id).ok())
                    .map_or(exchange.as_str(), |tt| tt.name.as_str()),
                amount: Money::from_minor(t.amount),
                cash_received: t.cash_received.map(Money::from_minor),
                change_given: Money::from_minor(t.change_given),
                reference: t.reference.as_deref(),
            })
            .collect();
        let customer = s
            .customer_id
            .as_deref()
            .and_then(|c| self.customer_record(c));
        let phone = customer
            .and_then(|c| c.phone.as_deref())
            .and_then(|p| Phone::parse(p).ok());
        let customer_block = customer.map(|c| match (s.tax_invoice, c.tax_id.as_deref()) {
            (true, Some(tax_id)) => ReceiptCustomerBlock::TaxInvoice(TaxInvoiceBlock {
                company_name: c.company_name.as_deref().unwrap_or(&c.name),
                tax_id,
                tax_address: c.tax_address.as_deref().unwrap_or_default(),
            }),
            _ => ReceiptCustomerBlock::Customer(ReceiptCustomer {
                name: &c.name,
                company_name: c.company_name.as_deref(),
                phone: phone.as_ref(),
                tier_name: None,
            }),
        });
        let saved = (s.change_saved > 0).then(|| ReceiptSaved {
            saved: Money::from_minor(s.change_saved),
            balance: Money::from_minor(customer.map_or(0, |c| c.balance)),
        });
        let footer = ["Terima kasih".to_owned()];
        let input = ReceiptInput {
            width: self.receipt_width(&s.branch_id),
            header: ReceiptHeader {
                lines: &[],
                branch_name: "Toko Jakarta",
                address: "Jl. Contoh No. 1, Jakarta",
                tax_id: None,
            },
            number: &s.number,
            posted_at: sultan_core::time::Timestamp::from_unix_millis(s.posted_at),
            tz: sultan_core::doc_number::BranchTimeZone::default(),
            register: "R1",
            cashier: &cashier,
            customer: customer_block,
            lines: &lines,
            totals: ReceiptTotals {
                subtotal: Money::from_minor(s.subtotal),
                discount: Money::from_minor(s.line_discount + s.cart_discount),
                tax: by_rate
                    .into_iter()
                    .map(|(rate_bp, (base, tax))| TaxLine { rate_bp, base, tax })
                    .collect(),
                tax_inclusive: s.tax_inclusive,
                cash_rounding: Money::from_minor(s.cash_rounding),
                total: Money::from_minor(s.paid),
            },
            tenders: &tenders,
            saved,
            footer: &footer,
            copy,
            voided,
        };
        build_receipt(&input, &|l| l.text(lang).to_owned())
    }

    fn void_sale(
        &mut self,
        token: &str,
        id: &str,
        body: &VoidRequest,
        override_token: Option<&str>,
    ) -> Result<Sale, ClientError> {
        let (user, session) = self.own_session(token)?;
        let i = self.sale_index(id)?;
        if self.sales.sales[i].1.summary.branch_id != BRANCH_ID {
            return Err(invalid("sale"));
        }
        let approver =
            self.manager_or_override(&user, "sale.post_void", None, id, override_token)?;
        let sale = &self.sales.sales[i].1;
        if sale.summary.status == "voided" {
            return Err(api(409, "conflict"));
        }
        if sale.summary.return_count > 0 {
            return Err(api(409, "has_returns"));
        }
        if !matches!(body.refund.as_str(), "cash" | "balance") {
            return Err(invalid("refund"));
        }
        let customer = sale.summary.customer_id.clone();
        if body.refund == "balance" && customer.is_none() {
            return Err(invalid("customer"));
        }
        let refund = sale.summary.paid;
        // The balance part goes back to the balance first (§7), the rest as asked.
        let reversed: i64 = sale
            .tenders
            .iter()
            .filter(|t| t.kind == "customer_balance")
            .map(|t| t.amount)
            .sum();
        let rest = refund - reversed;
        let (cash, to_balance) = if body.refund == "balance" {
            (0, rest)
        } else {
            (rest, 0)
        };
        if let Some(c) = &customer {
            if reversed > 0 {
                self.entry(c, "payment_reversed", reversed);
            }
            if to_balance > 0 {
                self.entry(c, "refund", to_balance);
            }
        }
        let mut tenders = Vec::new();
        if cash > 0 {
            tenders.push(RefundTender {
                tender_type_id: Some(TENDER_CASH.to_owned()),
                kind: "cash".to_owned(),
                amount: cash,
            });
        }
        if reversed + to_balance > 0 {
            tenders.push(RefundTender {
                tender_type_id: Some(TENDER_BALANCE.to_owned()),
                kind: "customer_balance".to_owned(),
                amount: reversed + to_balance,
            });
        }
        let now = self.now;
        let sale = &mut self.sales.sales[i].1;
        "voided".clone_into(&mut sale.summary.status);
        sale.summary.voided_at = Some(now);
        sale.void = Some(SaleVoid {
            register_id: REGISTER_ID.to_owned(),
            cashier_session_id: session,
            user_id: approver.unwrap_or(user.id),
            reason: body.reason.clone(),
            reason_text: body.reason_text.clone(),
            refund,
            payment_reversed: reversed,
            to_balance,
            cash,
            tenders,
            voided_at: now,
            ..SaleVoid::default()
        });
        let voided = sale.clone();
        self.cash_sales -= cash;
        Ok(voided)
    }

    fn cart_void(
        &mut self,
        token: &str,
        body: &CartVoidRequest,
        override_token: Option<&str>,
    ) -> Result<CartVoid, ClientError> {
        let (user, session) = self.own_session(token)?;
        let approver = match body.kind.as_str() {
            "line" if self.sales.line_void_override => self.manager_or_override(
                &user,
                "sale.void_line",
                None,
                &body.cart_id,
                override_token,
            )?,
            "line" => None,
            "cart" => {
                self.manager_or_override(&user, "sale.void", None, &body.cart_id, override_token)?
            }
            _ => {
                return Err(Problem::new(422, "invalid_field", "kind")
                    .with("field", "kind")
                    .into());
            }
        };
        let void = CartVoid {
            kind: body.kind.clone(),
            cashier_session_id: session,
            amount: body.lines.iter().map(|l| l.amount).sum(),
            approved_by: approver,
        };
        self.sales.voids.push(void.clone());
        Ok(void)
    }

    #[allow(clippy::too_many_lines)] // by receipt, without one, and the exchange
    fn post_return(
        &mut self,
        token: &str,
        id: &str,
        body: &ReturnRequest,
        override_token: Option<&str>,
    ) -> Result<SaleReturn, ClientError> {
        if let Some((request, stored)) = self.sales.returns.iter().find(|(_, r)| r.summary.id == id)
        {
            return if request == body {
                Ok(stored.clone())
            } else {
                Err(api(409, "conflict"))
            };
        }
        let (user, session) = self.own_session(token)?;
        let mut lines = Vec::new();
        let mut customer = body.customer_id.clone();
        if let Some(sale_id) = &body.sale_id {
            let i = self.sale_index(sale_id)?;
            let sale = &self.sales.sales[i].1;
            if sale.summary.status == "voided" {
                return Err(api(409, "conflict"));
            }
            if sale.summary.branch_id != BRANCH_ID {
                return Err(invalid("sale"));
            }
            if self.outside_window(sale.summary.posted_at) && !user.reports {
                self.window_override(id, override_token)?;
            }
            let sale = &self.sales.sales[i].1;
            customer = sale.summary.customer_id.clone().or(customer);
            let mut so_far: HashMap<u32, (i64, i64)> = HashMap::new();
            for l in &body.lines {
                let no = l.sale_line_no.unwrap_or_default();
                let sold = sale
                    .lines
                    .iter()
                    .find(|s| s.line_no == no)
                    .ok_or_else(|| api(422, "invalid_field"))?;
                let (q, r) = so_far.get(&no).copied().unwrap_or_default();
                let refund = line_refund(
                    SoldLine {
                        qty: Qty::from_thousandths(sold.qty_thousandths),
                        amount: Money::from_minor(sold.total),
                    },
                    ReturnedSoFar {
                        qty: Qty::from_thousandths(sold.returned_qty_thousandths + q),
                        refunded: Money::from_minor(sold.refunded + r),
                    },
                    Qty::from_thousandths(l.qty_thousandths),
                )
                .map_err(|e| rule(&e))?
                .minor();
                so_far.insert(no, (q + l.qty_thousandths, r + refund));
                lines.push((l, refund));
            }
            let sale = &mut self.sales.sales[i].1;
            for line in &mut sale.lines {
                if let Some((q, r)) = so_far.get(&line.line_no) {
                    line.returned_qty_thousandths += q;
                    line.refunded += r;
                }
            }
            sale.summary.return_count += 1;
        } else {
            let quote = self.quote(
                token,
                &QuoteRequest {
                    customer_id: customer.clone(),
                    lines: body
                        .lines
                        .iter()
                        .map(|l| QuoteLine::new(&l.variant_id, &l.unit_id, l.qty_thousandths))
                        .collect(),
                },
            )?;
            for (l, q) in body.lines.iter().zip(&quote.lines) {
                lines.push((l, q.tax.as_ref().map_or(0, |t| t.total)));
            }
            let total: i64 = lines.iter().map(|(_, r)| r).sum();
            self.manager_or_override(&user, "refund.no_receipt", Some(total), id, override_token)?;
        }
        let refund: i64 = lines.iter().map(|(_, r)| r).sum();
        let mut tenders = Vec::new();
        let mut exchange_sale = None;
        let mut rest = refund;
        if let Some(ex) = &body.exchange {
            let (sale, paid_by_refund) =
                self.build_sale(token, &ex.sale_id, &ex.sale, Some(refund), override_token)?;
            self.sales.sales.push((ex.sale.clone(), sale.clone()));
            if paid_by_refund > 0 {
                tenders.push(RefundTender {
                    tender_type_id: None,
                    kind: "exchange".to_owned(),
                    amount: paid_by_refund,
                });
            }
            rest -= paid_by_refund;
            exchange_sale = Some(sale);
        }
        if rest > 0 && body.refund == "balance" {
            let Some(c) = customer.clone() else {
                return Err(invalid("customer"));
            };
            self.entry(&c, "refund", rest);
            tenders.push(RefundTender {
                tender_type_id: Some(TENDER_BALANCE.to_owned()),
                kind: "customer_balance".to_owned(),
                amount: rest,
            });
        } else if rest > 0 {
            tenders.push(RefundTender {
                tender_type_id: Some(TENDER_CASH.to_owned()),
                kind: "cash".to_owned(),
                amount: rest,
            });
            self.cash_sales -= rest;
        }
        let stored = SaleReturn {
            summary: ReturnSummary {
                id: id.to_owned(),
                number: self.number("RT-"),
                status: "posted".to_owned(),
                sale_id: body.sale_id.clone(),
                branch_id: BRANCH_ID.to_owned(),
                register_id: REGISTER_ID.to_owned(),
                cashier_session_id: session,
                user_id: user.id,
                customer_name: self.customer_name(customer.as_deref()),
                customer_code: self.customer_code(customer.as_deref()),
                cashier_name: Some(user.name.clone()),
                register_code: Some("R1".to_owned()),
                customer_id: customer,
                reason: body.reason.clone(),
                refund,
                exchange_sale_id: exchange_sale.as_ref().map(|s: &Sale| s.summary.id.clone()),
                posted_at: self.now,
                ..ReturnSummary::default()
            },
            lines: lines
                .iter()
                .enumerate()
                .map(|(i, (l, r))| ReturnLine {
                    line_no: u32::try_from(i + 1).unwrap_or(u32::MAX),
                    sale_line_no: l.sale_line_no,
                    variant_id: l.variant_id.clone(),
                    unit_id: l.unit_id.clone(),
                    name: self
                        .line_names(&l.variant_id, &l.unit_id)
                        .map_or_else(|| l.variant_id.clone(), |(name, _)| name),
                    qty_thousandths: l.qty_thousandths,
                    refund: *r,
                    disposition: if l.write_off_reason.is_some() {
                        "write_off"
                    } else {
                        "restock"
                    }
                    .to_owned(),
                    write_off_reason: l.write_off_reason.clone(),
                })
                .collect(),
            tenders,
            exchange_sale,
        };
        self.sales.returns.push((body.clone(), stored.clone()));
        Ok(stored)
    }

    fn hold(&mut self, token: &str, id: &str, body: &HoldRequest) -> Result<HeldCart, ClientError> {
        let user = self.caller(token)?;
        if self
            .sales
            .held
            .iter()
            .any(|c| c.id == id && c.taken_by.is_some())
        {
            return Err(api(409, "cart_taken"));
        }
        self.sales.held.retain(|c| c.id != id);
        let cart = HeldCart {
            id: id.to_owned(),
            register_id: REGISTER_ID.to_owned(),
            user_id: user.id,
            customer_id: body.customer_id.clone(),
            note: body
                .note
                .as_deref()
                .map(str::trim)
                .filter(|n| !n.is_empty())
                .map(str::to_owned),
            payload: body.payload.clone(),
            item_count: body.item_count,
            total: body.total,
            held_at: self.now,
            taken_by: None,
        };
        self.sales.held.push(cart.clone());
        Ok(cart)
    }

    fn held_index(&self, id: &str) -> Result<usize, ClientError> {
        self.sales
            .held
            .iter()
            .position(|c| c.id == id)
            .ok_or_else(|| api(404, "not_found"))
    }

    fn take(&mut self, token: &str, id: &str) -> Result<HeldCart, ClientError> {
        let user = self.caller(token)?;
        let i = self.held_index(id)?;
        let cart = &mut self.sales.held[i];
        if cart.taken_by.is_some() {
            return Err(api(409, "cart_taken"));
        }
        cart.taken_by = Some(user.id);
        Ok(cart.clone())
    }

    fn drop_cart(
        &mut self,
        token: &str,
        id: &str,
        override_token: Option<&str>,
    ) -> Result<HeldCart, ClientError> {
        let user = self.caller(token)?;
        let i = self.held_index(id)?;
        if self.sales.held[i].taken_by.is_some() {
            return Err(api(409, "cart_taken"));
        }
        self.manager_or_override(&user, "sale.void", None, id, override_token)?;
        Ok(self.sales.held.remove(i))
    }

    fn history(&self, customer: &str, before: Option<&str>) -> HistoryPage {
        let mut items: Vec<HistoryItem> = self
            .sales
            .sales
            .iter()
            .filter(|(_, s)| s.summary.customer_id.as_deref() == Some(customer))
            .map(|(_, s)| HistoryItem {
                kind: "sale".to_owned(),
                document_id: s.summary.id.clone(),
                number: s.summary.number.clone(),
                branch_id: s.summary.branch_id.clone(),
                customer_id: customer.to_owned(),
                customer_name: self.customer_name(Some(customer)).unwrap_or_default(),
                customer_code: self.customer_code(Some(customer)).unwrap_or_default(),
                posted_at: s.summary.posted_at,
                total: s.summary.paid,
                voided: s.summary.status == "voided",
            })
            .chain(
                self.sales
                    .returns
                    .iter()
                    .filter(|(_, r)| r.summary.customer_id.as_deref() == Some(customer))
                    .map(|(_, r)| HistoryItem {
                        kind: "return".to_owned(),
                        document_id: r.summary.id.clone(),
                        number: r.summary.number.clone(),
                        branch_id: r.summary.branch_id.clone(),
                        customer_id: customer.to_owned(),
                        customer_name: self.customer_name(Some(customer)).unwrap_or_default(),
                        customer_code: self.customer_code(Some(customer)).unwrap_or_default(),
                        posted_at: r.summary.posted_at,
                        total: r.summary.refund,
                        voided: false,
                    }),
            )
            .collect();
        items.sort_by(|a, b| b.posted_at.cmp(&a.posted_at).then(b.number.cmp(&a.number)));
        let start = before
            .and_then(|b| items.iter().position(|i| i.document_id == b))
            .map_or(0, |i| i + 1);
        let page: Vec<HistoryItem> = items.into_iter().skip(start).collect();
        let next_before_id =
            (page.len() > HISTORY_PAGE).then(|| page[HISTORY_PAGE - 1].document_id.clone());
        HistoryPage {
            items: page.into_iter().take(HISTORY_PAGE).collect(),
            next_before_id,
        }
    }

    fn put_tender(
        &mut self,
        token: &str,
        id: &str,
        body: &TenderTypeBody,
    ) -> Result<TenderType, ClientError> {
        if !self.bo_user(token)?.manage {
            return Err(Problem::new(403, "permission_denied", "denied")
                .with("permission", crate::sales::TENDER_MANAGE)
                .into());
        }
        let invalid = |field: &str| -> ClientError {
            Problem::new(422, "invalid_field", field)
                .with("field", field)
                .into()
        };
        let name = body.name.trim();
        if name.is_empty() || name.chars().count() > 40 {
            return Err(invalid("name"));
        }
        if body.kind == "exchange" || body.kind.parse::<sultan_core::sale::TenderKind>().is_err() {
            return Err(invalid("kind"));
        }
        let tender = TenderType {
            id: id.to_owned(),
            name: name.to_owned(),
            kind: body.kind.clone(),
            needs_reference: body.needs_reference,
            opens_drawer: body.opens_drawer,
            active: body.active,
            sort: body.sort,
            archived: false,
        };
        match self.sales.tenders.iter_mut().find(|t| t.id == id) {
            Some(t) if t.archived => Err(api(409, "archived")),
            Some(t) if t.kind != body.kind => Err(invalid("kind")),
            Some(t) => {
                *t = tender.clone();
                Ok(tender)
            }
            None => {
                self.sales.tenders.push(tender.clone());
                Ok(tender)
            }
        }
    }

    fn archive_tender(&mut self, token: &str, id: &str) -> Result<TenderType, ClientError> {
        if !self.bo_user(token)?.manage {
            return Err(Problem::new(403, "permission_denied", "denied")
                .with("permission", crate::sales::TENDER_MANAGE)
                .into());
        }
        let t = self
            .sales
            .tenders
            .iter_mut()
            .find(|t| t.id == id)
            .ok_or_else(|| api(404, "not_found"))?;
        t.archived = true;
        Ok(t.clone())
    }

    fn any_caller(&self, token: &str) -> Result<(), ClientError> {
        if self.caller(token).is_ok() {
            return Ok(());
        }
        self.bo_user(token).map(|_| ())
    }
}

/// One page of `all` (newest first) after `query.before_id`, at most `query.limit` (default 50),
/// and the next page's cursor when more follow.
fn page_of<T>(
    all: Vec<T>,
    query: &DocQuery,
    id: impl Fn(&T) -> &String,
) -> (Vec<T>, Option<String>) {
    let start = query
        .before_id
        .as_ref()
        .and_then(|b| all.iter().position(|x| id(x) == b))
        .map_or(0, |i| i + 1);
    let limit = query.limit.unwrap_or(50) as usize;
    let rest: Vec<T> = all.into_iter().skip(start).collect();
    let more = rest.len() > limit;
    let items: Vec<T> = rest.into_iter().take(limit).collect();
    let next = if more {
        items.last().map(|x| id(x).clone())
    } else {
        None
    };
    (items, next)
}

fn run<T: Send + 'static>(
    fake: &FakeService,
    method: &'static str,
    detail: String,
    body: impl FnOnce(&mut State) -> Result<T, ClientError>,
) -> crate::BoxFuture<T> {
    let (mut s, failure) = fake.begin(method, detail);
    ready(match failure {
        Some(e) => Err(e),
        None => body(&mut s),
    })
}

impl FakeService {
    /// Makes a line void need a manager (`sale.void_line` above the cashier, spec 2.6 §3).
    pub fn require_line_void_override(&self, on: bool) {
        self.state().sales.line_void_override = on;
    }

    /// The posted sales with their request bodies, oldest first.
    #[must_use]
    pub fn posted_sales(&self) -> Vec<(SaleRequest, Sale)> {
        self.state().sales.sales.clone()
    }

    /// The cart voids recorded so far (what the close report would count).
    #[must_use]
    pub fn cart_voids(&self) -> Vec<CartVoid> {
        self.state().sales.voids.clone()
    }

    pub(super) fn tender_types_call(
        &self,
        token: &str,
        all: bool,
    ) -> crate::BoxFuture<Vec<TenderType>> {
        run(self, "tender_types", all.to_string(), |s| {
            s.any_caller(token)?;
            let mut list: Vec<TenderType> = s
                .sales
                .tenders
                .iter()
                .filter(|t| all || (t.active && !t.archived))
                .cloned()
                .collect();
            list.sort_by_key(|t| t.sort);
            Ok(list)
        })
    }

    pub(super) fn post_sale_call(
        &self,
        token: &str,
        id: &str,
        body: &SaleRequest,
        override_token: Option<&str>,
    ) -> crate::BoxFuture<Sale> {
        let detail = format!("{id} override={}", override_token.unwrap_or("-"));
        run(self, "post_sale", detail, |s| {
            s.post_sale(token, id, body, override_token)
        })
    }

    pub(super) fn sale_call(&self, token: &str, id: &str) -> crate::BoxFuture<Sale> {
        run(self, "sale", id.to_owned(), |s| {
            s.any_caller(token)?;
            Ok(s.sales.sales[s.sale_index(id)?].1.clone())
        })
    }

    pub(super) fn sales_call(&self, token: &str, query: &DocQuery) -> crate::BoxFuture<SalePage> {
        let detail = serde_json::to_string(query).unwrap_or_default();
        run(self, "sales", detail, |s| {
            s.any_caller(token)?;
            Ok(s.list_sales(query))
        })
    }

    pub(super) fn receipt_call(
        &self,
        token: &str,
        id: &str,
        copy: bool,
        lang: &str,
        override_token: Option<&str>,
    ) -> crate::BoxFuture<Receipt> {
        run(
            self,
            "receipt",
            format!("{id} copy={copy} lang={lang}"),
            |s| s.receipt(token, id, copy, lang, override_token),
        )
    }

    pub(super) fn void_sale_call(
        &self,
        token: &str,
        id: &str,
        body: &VoidRequest,
        key: &str,
        override_token: Option<&str>,
    ) -> crate::BoxFuture<Sale> {
        run(self, "void_sale", format!("{id} key={key}"), |s| {
            if let Some(stored) = s.sales.keyed.get(key) {
                return Ok(stored.clone());
            }
            let sale = s.void_sale(token, id, body, override_token)?;
            s.sales.keyed.insert(key.to_owned(), sale.clone());
            Ok(sale)
        })
    }

    pub(super) fn cart_void_call(
        &self,
        token: &str,
        body: &CartVoidRequest,
        key: &str,
        override_token: Option<&str>,
    ) -> crate::BoxFuture<CartVoid> {
        run(self, "cart_void", format!("{} key={key}", body.kind), |s| {
            s.cart_void(token, body, override_token)
        })
    }

    pub(super) fn drawer_call(
        &self,
        token: &str,
        body: &crate::DrawerRequest,
        key: &str,
        override_token: Option<&str>,
    ) -> crate::BoxFuture<crate::DrawerOpen> {
        run(self, "open_drawer", format!("key={key}"), |s| {
            s.drawer(token, body, override_token)
        })
    }

    /// The recorded drawer opens.
    #[must_use]
    pub fn drawer_opens(&self) -> Vec<crate::DrawerOpen> {
        self.state().sales.drawer.clone()
    }

    /// Moves a posted sale to another branch (the branch rule of §19 item 40).
    pub fn move_sale_to_branch(&self, id: &str, branch: &str) {
        let mut s = self.state();
        if let Some((_, sale)) = s.sales.sales.iter_mut().find(|(_, x)| x.summary.id == id) {
            branch.clone_into(&mut sale.summary.branch_id);
        }
    }

    pub(super) fn post_return_call(
        &self,
        token: &str,
        id: &str,
        body: &ReturnRequest,
        override_token: Option<&str>,
    ) -> crate::BoxFuture<SaleReturn> {
        run(self, "post_return", id.to_owned(), |s| {
            s.post_return(token, id, body, override_token)
        })
    }

    pub(super) fn hold_call(
        &self,
        token: &str,
        id: &str,
        body: &HoldRequest,
    ) -> crate::BoxFuture<HeldCart> {
        run(self, "hold_cart", id.to_owned(), |s| {
            s.hold(token, id, body)
        })
    }

    pub(super) fn held_carts_call(&self, token: &str) -> crate::BoxFuture<Vec<HeldCart>> {
        run(self, "held_carts", String::new(), |s| {
            s.caller(token)?;
            let mut list: Vec<HeldCart> = s
                .sales
                .held
                .iter()
                .filter(|c| c.taken_by.is_none())
                .cloned()
                .collect();
            list.reverse();
            Ok(list)
        })
    }

    pub(super) fn take_call(&self, token: &str, id: &str) -> crate::BoxFuture<HeldCart> {
        run(self, "take_held_cart", id.to_owned(), |s| s.take(token, id))
    }

    pub(super) fn drop_cart_call(
        &self,
        token: &str,
        id: &str,
        override_token: Option<&str>,
    ) -> crate::BoxFuture<HeldCart> {
        run(self, "delete_held_cart", id.to_owned(), |s| {
            s.drop_cart(token, id, override_token)
        })
    }

    pub(super) fn history_call(
        &self,
        token: &str,
        id: &str,
        before: Option<&str>,
    ) -> crate::BoxFuture<HistoryPage> {
        run(self, "customer_history", id.to_owned(), |s| {
            s.any_caller(token)?;
            Ok(s.history(id, before))
        })
    }

    pub(super) fn recent_call(&self, token: &str, id: &str) -> crate::BoxFuture<Vec<SaleSummary>> {
        run(self, "recent_receipts", id.to_owned(), |s| {
            s.any_caller(token)?;
            Ok(s.sales
                .sales
                .iter()
                .rev()
                .map(|(_, x)| x.summary.clone())
                .filter(|x| x.customer_id.as_deref() == Some(id) && x.status == "posted")
                .take(5)
                .collect())
        })
    }

    pub(super) fn put_tender_call(
        &self,
        token: &str,
        id: &str,
        body: &TenderTypeBody,
    ) -> crate::BoxFuture<TenderType> {
        run(self, "put_tender_type", id.to_owned(), |s| {
            s.put_tender(token, id, body)
        })
    }

    pub(super) fn archive_tender_call(
        &self,
        token: &str,
        id: &str,
    ) -> crate::BoxFuture<TenderType> {
        run(self, "delete_tender_type", id.to_owned(), |s| {
            s.archive_tender(token, id)
        })
    }

    pub(super) fn returns_call(
        &self,
        token: &str,
        query: &DocQuery,
    ) -> crate::BoxFuture<ReturnPage> {
        let detail = serde_json::to_string(query).unwrap_or_default();
        run(self, "returns", detail, |s| {
            s.any_caller(token)?;
            let all: Vec<ReturnSummary> = s
                .sales
                .returns
                .iter()
                .rev()
                .map(|(_, r)| r.summary.clone())
                .filter(|r| {
                    query
                        .cashier_session_id
                        .as_ref()
                        .is_none_or(|q| &r.cashier_session_id == q)
                        && query
                            .customer_id
                            .as_ref()
                            .is_none_or(|q| r.customer_id.as_ref() == Some(q))
                        && query.number.as_ref().is_none_or(|q| &r.number == q)
                        && query.status.as_ref().is_none_or(|q| &r.status == q)
                        && query.branch_id.as_ref().is_none_or(|q| &r.branch_id == q)
                        && query
                            .register_id
                            .as_ref()
                            .is_none_or(|q| &r.register_id == q)
                        && query.from.is_none_or(|f| r.posted_at >= f)
                        && query.until.is_none_or(|u| r.posted_at < u)
                })
                .collect();
            let (items, next_before_id) = page_of(all, query, |r| &r.id);
            Ok(ReturnPage {
                items,
                next_before_id,
            })
        })
    }

    pub(super) fn return_receipt_call(
        &self,
        token: &str,
        id: &str,
        copy: bool,
        lang: &str,
        override_token: Option<&str>,
    ) -> crate::BoxFuture<Receipt> {
        run(
            self,
            "return_receipt",
            format!("{id} copy={copy} lang={lang}"),
            |s| s.return_receipt(token, id, copy, lang, override_token),
        )
    }

    pub(super) fn sales_settings_call(&self, token: &str) -> crate::BoxFuture<SalesSettings> {
        run(self, "sales_settings", String::new(), |s| {
            s.any_caller(token)?;
            Ok(s.sales.settings.clone())
        })
    }

    pub(super) fn put_sales_settings_call(
        &self,
        token: &str,
        body: &SalesSettings,
    ) -> crate::BoxFuture<SalesSettings> {
        run(
            self,
            "put_sales_settings",
            serde_json::to_string(body).unwrap_or_default(),
            |s| {
                if !s.bo_user(token)?.manage {
                    return Err(Problem::new(403, "permission_denied", "denied")
                        .with("permission", crate::sales::TENDER_MANAGE)
                        .into());
                }
                let invalid = |field: &str| -> ClientError {
                    Problem::new(422, "invalid_field", field)
                        .with("field", field)
                        .into()
                };
                if body.return_days > 3_650 {
                    return Err(invalid("return_days"));
                }
                for w in &body.receipt_widths {
                    if ![32, 42, 48].contains(&w.receipt_width) {
                        return Err(invalid("receipt_width"));
                    }
                }
                let settings = &mut s.sales.settings;
                settings.return_days = body.return_days;
                for w in &body.receipt_widths {
                    match settings
                        .receipt_widths
                        .iter_mut()
                        .find(|x| x.branch_id == w.branch_id)
                    {
                        Some(x) => x.receipt_width = w.receipt_width,
                        None => return Err(api(404, "not_found")),
                    }
                }
                Ok(settings.clone())
            },
        )
    }

    pub(super) fn return_call(&self, token: &str, id: &str) -> crate::BoxFuture<SaleReturn> {
        run(self, "sale_return", id.to_owned(), |s| {
            s.any_caller(token)?;
            s.sales
                .returns
                .iter()
                .find(|(_, r)| r.summary.id == id)
                .map(|(_, r)| r.clone())
                .ok_or_else(|| api(404, "not_found"))
        })
    }
}

/// What [`FakeService::seed_sales_demo`] posted.
#[derive(Debug, Clone)]
pub struct SalesDemo {
    /// The posted sales, oldest first (the second is voided, the third has a customer and a
    /// return).
    pub sales: Vec<Sale>,
    /// The return of the third sale.
    pub sale_return: SaleReturn,
    /// The customer of the third sale (a record named "Ibu Emas", code `G0LD0001`).
    pub customer: String,
}

impl FakeService {
    /// Posts a small sales history through the register calls (the back office's sales screens
    /// and their screenshots, phase S6): ani opens a session and sells `count` (at least 3)
    /// sales of Teh Botol; the second is post-voided with Budi's approval, the third is for the
    /// Gold customer and one of its 2 items comes back by receipt. Seeds the pricing demo.
    ///
    /// # Panics
    /// When a register call fails (a broken fake).
    #[allow(clippy::too_many_lines)] // one scripted register session
    pub async fn seed_sales_demo(&self, count: usize) -> SalesDemo {
        use crate::{
            OverrideRequest, ReturnLineRequest, SaleLineRequest, Service as _, TenderRequest,
        };
        let demo = self.seed_pricing_demo();
        self.seed_customer(crate::Customer {
            id: demo.gold_customer.clone(),
            code: "G0LD0001".to_owned(),
            display_code: "G0LD-0001".to_owned(),
            name: "Ibu Emas".to_owned(),
            phone: Some("+6281234500001".to_owned()),
            ..crate::Customer::default()
        });
        let variant = demo.teh.variants[0].id.clone();
        let unit = demo.teh.base_unit_id.clone();
        let cashier = self.pin_login("u-ani", "1234").await.unwrap().token;
        if self.current_session(&cashier).await.unwrap().is_none() {
            self.open_session(&cashier, 0, "seed-open").await.unwrap();
        }
        let mut sales = Vec::new();
        for i in 0..count.max(3) {
            let qty = i64::try_from(i % 3 + 1).unwrap_or(1);
            let mut body = SaleRequest {
                lines: vec![SaleLineRequest {
                    variant_id: variant.clone(),
                    unit_id: unit.clone(),
                    qty_thousandths: qty * 1000,
                    ..SaleLineRequest::default()
                }],
                tenders: vec![TenderRequest {
                    tender_type_id: TENDER_CASH.to_owned(),
                    amount: 20_000,
                    reference: None,
                }],
                ..SaleRequest::default()
            };
            if i == 2 {
                body.customer_id = Some(demo.gold_customer.clone());
            }
            let id = format!("seed-sale-{i:03}");
            let stale = self
                .post_sale(&cashier, &id, &body, None)
                .await
                .unwrap_err();
            body.totals = stale.problem().and_then(Problem::quote_totals).unwrap();
            sales.push(self.post_sale(&cashier, &id, &body, None).await.unwrap());
            let now = self.state().now;
            self.set_now(now + 7 * 60_000);
        }
        let voided = sales[1].summary.id.clone();
        let grant = self
            .request_override(
                &cashier,
                &OverrideRequest {
                    action: "sale.post_void".to_owned(),
                    value: None,
                    context_ref: voided.clone(),
                    approver_id: "u-budi".to_owned(),
                    approver_pin: "9999".to_owned(),
                },
            )
            .await
            .unwrap()
            .token;
        sales[1] = self
            .void_sale(
                &cashier,
                &voided,
                &VoidRequest {
                    reason: "wrong_item".to_owned(),
                    reason_text: None,
                    refund: "cash".to_owned(),
                },
                "seed-void",
                Some(&grant),
            )
            .await
            .unwrap();
        let sale_return = self
            .post_return(
                &cashier,
                "seed-return-001",
                &ReturnRequest {
                    sale_id: Some(sales[2].summary.id.clone()),
                    refund: "cash".to_owned(),
                    lines: vec![ReturnLineRequest {
                        sale_line_no: Some(1),
                        variant_id: variant,
                        unit_id: unit,
                        qty_thousandths: 1_000,
                        write_off_reason: None,
                    }],
                    ..ReturnRequest::default()
                },
                None,
            )
            .await
            .unwrap();
        sales[2] = self.sale(&cashier, &sales[2].summary.id).await.unwrap();
        SalesDemo {
            sales,
            sale_return,
            customer: demo.gold_customer,
        }
    }
}
