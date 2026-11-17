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
//! **Simplified:** no stock, no cash rounding, refunds go to cash only, the receipt text is
//! short (number, lines, TOTAL, COPY / VOID), the return window is not checked.

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
    SalePage, SaleRequest, SaleReturn, SaleSummary, SaleTender, SaleTotals, SaleVoid, TenderType,
    TenderTypeBody, VoidRequest,
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
    keyed: HashMap<String, Sale>,
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
            keyed: HashMap::new(),
        }
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
        format!("JKT01-R1-{kind}261116-{n:04}")
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
        let most = totals
            .lines
            .iter()
            .map(sultan_core::sale::LineTotals::line_discount_bp)
            .chain([totals.cart_discount_bp()])
            .max()
            .unwrap_or(0);
        if i64::from(most) > user.discount_limit {
            self.manager_or_override(
                &user,
                "sale.discount",
                Some(i64::from(most)),
                id,
                override_token,
            )?;
        }
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
                name: l.variant_id.clone(),
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
        let items: Vec<SaleSummary> = self
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
            })
            .collect();
        SalePage {
            items,
            next_before_id: None,
        }
    }

    fn receipt(
        &mut self,
        token: &str,
        id: &str,
        copy: bool,
        override_token: Option<&str>,
    ) -> Result<Receipt, ClientError> {
        let user = self.caller(token)?;
        let sale = self.sales.sales[self.sale_index(id)?].1.clone();
        if copy {
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
        let voided = sale.summary.status == "voided";
        let mut lines = Vec::new();
        if copy {
            lines.push("COPY".to_owned());
        }
        if voided {
            lines.push("VOID".to_owned());
        }
        lines.push(sale.summary.number.clone());
        for l in &sale.lines {
            lines.push(format!("{} {}", l.name, l.total));
        }
        lines.push(format!("TOTAL {}", sale.summary.paid));
        if voided {
            lines.push("VOID".to_owned());
        }
        let text = lines.iter().fold(String::new(), |mut t, l| {
            t.push_str(l);
            t.push('\n');
            t
        });
        Ok(Receipt {
            sale_id: sale.summary.id.clone(),
            number: sale.summary.number.clone(),
            copy,
            voided,
            lines,
            text,
            barcode: sale.summary.number,
        })
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
            return Err(Problem::new(422, "invalid_field", "refund")
                .with("field", "refund")
                .into());
        }
        let refund = sale.summary.paid;
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
            cash: refund,
            tenders: vec![RefundTender {
                tender_type_id: Some(TENDER_CASH.to_owned()),
                kind: "cash".to_owned(),
                amount: refund,
            }],
            voided_at: now,
            ..SaleVoid::default()
        });
        let voided = sale.clone();
        self.cash_sales -= refund;
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
        if rest > 0 {
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
                    name: l.variant_id.clone(),
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

    fn history(&self, customer: &str) -> HistoryPage {
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
                        posted_at: r.summary.posted_at,
                        total: r.summary.refund,
                        voided: false,
                    }),
            )
            .collect();
        items.sort_by(|a, b| b.posted_at.cmp(&a.posted_at).then(b.number.cmp(&a.number)));
        HistoryPage {
            items,
            next_before_id: None,
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
        override_token: Option<&str>,
    ) -> crate::BoxFuture<Receipt> {
        run(self, "receipt", format!("{id} copy={copy}"), |s| {
            s.receipt(token, id, copy, override_token)
        })
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

    pub(super) fn history_call(&self, token: &str, id: &str) -> crate::BoxFuture<HistoryPage> {
        run(self, "customer_history", id.to_owned(), |s| {
            s.any_caller(token)?;
            Ok(s.history(id))
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
            Ok(ReturnPage {
                items: s
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
                    })
                    .collect(),
                next_before_id: None,
            })
        })
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
