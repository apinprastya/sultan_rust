//! The register after the sale (spec 2.6 §4, §7–§9, §13; phase S5), as more of
//! [`Controller`]'s state machine. Its dialogs are [`Modal`]s of the sell screen.
//!
//! - **Hold** (F11): an optional note, then `PUT /held-carts/{id}` with the cart as the crash
//!   copy has it ([`crate::sell::Saved`], plus the register and cashier names for the list); a new
//!   empty cart follows. **Resume** (Shift+F11): every held cart of the branch, newest first
//!   (register, cashier, time, customer, items, total, note). Enter takes it
//!   (`POST /held-carts/{id}/take`; `409 cart_taken` says so and reloads the list) into an empty
//!   cart under a new cart ID, and quotes it again (manual prices stay as they were). With lines
//!   in the cart, Shift+F11 offers to hold them first. Del drops a held cart (`sale.void`, a
//!   manager override with the held cart's ID). `cart.held` / `cart.taken` reload an open list.
//! - **Last receipts** (F6): this session's sales, newest first, and a field for a receipt
//!   number (typed or scanned, `GET /sales/by-number`). Enter shows the receipt in the window
//!   language; there R reprints it (`copy=true`; anything but the session's last sale needs
//!   `sale.reprint`, overridable), V post-voids it (a reason, the refund in cash or, for a sale
//!   with a customer, to their balance; `sale.post_void`, overridable) and T starts its return.
//! - **Return / exchange** (F10, an empty cart): a receipt number, a pick from the attached
//!   customer's recent receipts, or F2 for a return without a receipt (items are scanned; the
//!   refund is the current price for the attached customer, quoted). The lines show what can
//!   still be returned; `+` / `−` / `*` pick the quantity (by receipt, Q or a digit types it, with
//!   3 decimals for a weighed line), Del writes a line off (with a reason),
//!   Tab switches the refund between cash and the customer's balance. Enter posts the return
//!   (`PUT /returns/{id}`); `return_window` and the `refund.no_receipt` limit go through the
//!   override dialog. F9 starts an exchange instead: the refund becomes the credit of a new cart
//!   whose payment starts with it (showing what is paid back when the credit is larger), and the
//!   return is posted together with the new sale.
//! - **Purchase history**: F9 on a customer in the customer dialog lists their sales (VOID marked)
//!   and returns, newest first, ten at a time (F8 loads more); Enter shows a sale's receipt.
//! - **Drawer** (F5): `POST /drawer/open` (`drawer.open`, overridable with the session ID).

use serde_json::json;
use sultan_client::{
    ClientError, Customer, DrawerOpen, HeldCart, HistoryPage, Lookup, ProtectedAction, Quote,
    QuoteLine, QuoteRequest, Receipt, ReturnLineRequest, ReturnRequest, Sale, SalePage, SaleReturn,
    SaleSummary, VoidRequest,
};
use sultan_core::catalog::{Qty, UnitKind};
use sultan_core::money::Money;
use sultan_core::sale::{ReturnedSoFar, SaleError, SoldLine, check_qty, line_refund};

use crate::amount::rupiah;
use crate::controller::{
    Action, Controller, Effect, Notice, Screen, Shortcut, Tone, ends_login, new_key, notice_for,
};
use crate::input::Key;
use crate::sell::Saved;
use crate::till::{Modal, Retry, VOID_REASONS};

/// A service call of this module; the answer comes back as [`Reply`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Call {
    /// `PUT /held-carts/{id}` (the payload as JSON text).
    Hold {
        /// The held cart's ID (the cart's).
        id: String,
        /// The attached customer.
        customer_id: Option<String>,
        /// The note.
        note: Option<String>,
        /// The cart (JSON).
        payload: String,
        /// Items.
        item_count: u32,
        /// Total.
        total: i64,
    },
    /// `GET /held-carts`.
    HeldCarts,
    /// `POST /held-carts/{id}/take`.
    Take {
        /// The held cart.
        id: String,
        /// Idempotency key.
        key: String,
    },
    /// `DELETE /held-carts/{id}`.
    DropHeld {
        /// The held cart.
        id: String,
        /// `X-Override`.
        override_token: Option<String>,
    },
    /// `GET /sales?cashier_session_id=`.
    SessionSales(String),
    /// `GET /sales/by-number/{number}`.
    SaleByNumber(String),
    /// `GET /sales/{id}`.
    Sale(String),
    /// `GET /sales/{id}/receipt` in the window language.
    Receipt {
        /// The sale.
        id: String,
        /// A reprint.
        copy: bool,
        /// `X-Override`.
        override_token: Option<String>,
    },
    /// `POST /sales/{id}/void`.
    Void {
        /// The sale.
        id: String,
        /// The body.
        body: VoidRequest,
        /// Idempotency key.
        key: String,
        /// `X-Override`.
        override_token: Option<String>,
    },
    /// `PUT /returns/{id}`.
    Return {
        /// The return.
        id: String,
        /// The body.
        body: Box<ReturnRequest>,
        /// `X-Override` (comma separated).
        override_token: Option<String>,
    },
    /// `PUT /returns/{id}` of an exchange (the return with the new sale).
    Exchange {
        /// The return.
        id: String,
        /// The body, with `exchange`.
        body: Box<ReturnRequest>,
        /// `X-Override` (comma separated).
        override_token: Option<String>,
    },
    /// `GET /customers/{id}/recent-receipts`.
    Recent(String),
    /// `GET /customers/{id}/history`.
    History {
        /// The customer.
        customer_id: String,
        /// The page cursor.
        before: Option<String>,
    },
    /// `GET /catalog/lookup` of an item returned without a receipt.
    Lookup(String),
    /// `POST /pricing/quote` of the items returned without a receipt, numbered by
    /// [`Return::quote_seq`] so only the latest answer is applied.
    Quote(u64, QuoteRequest),
    /// `POST /drawer/open`.
    Drawer {
        /// Idempotency key.
        key: String,
        /// `X-Override`.
        override_token: Option<String>,
    },
}

/// The answer to a [`Call`].
#[derive(Debug, Clone, PartialEq)]
pub enum Reply {
    /// [`Call::Hold`].
    Held(Result<HeldCart, ClientError>),
    /// [`Call::HeldCarts`].
    HeldCarts(Result<Vec<HeldCart>, ClientError>),
    /// [`Call::Take`].
    Taken(Result<HeldCart, ClientError>),
    /// [`Call::DropHeld`].
    Dropped(Result<HeldCart, ClientError>),
    /// [`Call::SessionSales`].
    Sales(Result<SalePage, ClientError>),
    /// [`Call::SaleByNumber`] and [`Call::Sale`].
    Sale(Result<Sale, ClientError>),
    /// [`Call::Receipt`].
    Receipt(Result<Receipt, ClientError>),
    /// [`Call::Void`].
    Voided(Result<Sale, ClientError>),
    /// [`Call::Return`].
    Returned(Result<SaleReturn, ClientError>),
    /// [`Call::Exchange`].
    Exchanged(Result<SaleReturn, ClientError>),
    /// [`Call::Recent`].
    Recent(Result<Vec<SaleSummary>, ClientError>),
    /// [`Call::History`].
    History(Result<HistoryPage, ClientError>),
    /// [`Call::Lookup`].
    Looked(Result<Lookup, ClientError>),
    /// [`Call::Quote`], with its number.
    Quoted(u64, Result<Quote, ClientError>),
    /// [`Call::Drawer`].
    Drawer(Result<DrawerOpen, ClientError>),
}

impl Reply {
    fn error(&self) -> Option<&ClientError> {
        match self {
            Self::Held(r) | Self::Taken(r) | Self::Dropped(r) => r.as_ref().err(),
            Self::HeldCarts(r) => r.as_ref().err(),
            Self::Sales(r) => r.as_ref().err(),
            Self::Sale(r) | Self::Voided(r) => r.as_ref().err(),
            Self::Receipt(r) => r.as_ref().err(),
            Self::Returned(r) | Self::Exchanged(r) => r.as_ref().err(),
            Self::Recent(r) => r.as_ref().err(),
            Self::History(r) => r.as_ref().err(),
            Self::Looked(r) => r.as_ref().err(),
            Self::Quoted(_, r) => r.as_ref().err(),
            Self::Drawer(r) => r.as_ref().err(),
        }
    }
}

/// The hold note (F11), and whether the resume list follows.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HoldNote {
    /// The note.
    pub text: String,
    /// Shift+F11 asked to hold first: the resume list opens after holding.
    pub then_resume: bool,
}

/// A held cart in the resume list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeldRow {
    /// ID.
    pub id: String,
    /// The register's name (from the payload; the ID otherwise).
    pub register: String,
    /// The cashier's name.
    pub cashier: String,
    /// Held at (unix ms).
    pub at: i64,
    /// The customer's name.
    pub customer: String,
    /// Items.
    pub items: u32,
    /// Total.
    pub total: i64,
    /// Note.
    pub note: String,
}

/// The resume list (Shift+F11).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HeldList {
    /// Newest first.
    pub rows: Vec<HeldRow>,
    /// The highlighted cart.
    pub cursor: usize,
}

/// A document of the last receipts or the purchase history.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Doc {
    /// `sale` or `return`.
    pub kind: String,
    /// ID.
    pub id: String,
    /// Number.
    pub number: String,
    /// Posted (unix ms).
    pub at: i64,
    /// Paid or refunded.
    pub total: i64,
    /// The sale has a customer.
    pub customer: bool,
    /// Voided.
    pub voided: bool,
    /// Branch.
    pub branch: String,
}

/// The last receipts (F6) or a customer's purchase history.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Docs {
    /// The history of this customer (ID, name); `None` = this session's sales.
    pub history: Option<(String, String)>,
    /// Newest first.
    pub rows: Vec<Doc>,
    /// The highlighted row.
    pub cursor: usize,
    /// A typed or scanned receipt number.
    pub number: String,
    /// The next history page.
    pub next: Option<String>,
}

/// A receipt on screen.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Preview {
    /// The sale, once loaded.
    pub sale: Option<Box<Sale>>,
    /// The sale's ID.
    pub id: String,
    /// The receipt text.
    pub lines: Vec<String>,
    /// The list it was opened from (Esc goes back).
    pub back: Option<Box<Docs>>,
    /// After a post-void: cash paid out and refunded to the balance.
    pub refund: Option<(i64, i64)>,
}

/// The post-void dialog (§7).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PostVoid {
    /// The sale.
    pub sale: Box<Sale>,
    /// The highlighted reason.
    pub cursor: usize,
    /// The text of `other`.
    pub text: String,
    /// Refund to the customer's balance (else cash).
    pub balance: bool,
    /// The idempotency key.
    pub key: String,
    /// The list the receipt came from.
    pub back: Option<Box<Docs>>,
}

/// What is left of a line returned without a receipt: no cap, but small enough that `+` never
/// overflows. Never a quantity itself (`*` is not offered without a receipt).
const NO_RECEIPT_LEFT: i64 = i64::MAX / 4;

/// A returned line.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReturnLine {
    /// The sale line (by receipt).
    pub sale_line_no: Option<u32>,
    /// The variant.
    pub variant_id: String,
    /// The unit.
    pub unit_id: String,
    /// Name.
    pub name: String,
    /// Unit name.
    pub unit: String,
    /// Sold (by receipt), thousandths.
    pub sold: i64,
    /// What can still be returned, thousandths ([`NO_RECEIPT_LEFT`] without a receipt).
    pub left: i64,
    /// The quantity returned now, thousandths.
    pub qty: i64,
    /// What the sale line cost (its total) and what earlier returns took (qty, refund).
    pub paid: (i64, i64, i64),
    /// The refund of `qty` (`core::sale::line_refund`, or the quote without a receipt).
    pub refund: i64,
    /// Written off with this reason.
    pub write_off: Option<String>,
    /// A decimal unit (a weighed line): a typed quantity may have 3 decimals (else whole).
    pub decimal: bool,
}

/// Where the return dialog is.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ReturnStep {
    /// Which sale (a number, a recent receipt, or none).
    #[default]
    Find,
    /// The lines.
    Lines,
    /// Posted.
    Done,
}

/// The return dialog (F10).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Return {
    /// The return's ID (`context_ref` of its overrides).
    pub id: String,
    /// The step.
    pub step: ReturnStep,
    /// A typed receipt number (Find) or item code (Lines without a receipt).
    pub text: String,
    /// The attached customer's recent receipts.
    pub recent: Vec<SaleSummary>,
    /// The highlighted recent receipt or line.
    pub cursor: usize,
    /// The sale returned by receipt.
    pub sale: Option<Box<Sale>>,
    /// A return without a receipt.
    pub no_receipt: bool,
    /// The lines.
    pub lines: Vec<ReturnLine>,
    /// Refund to the customer's balance (else cash).
    pub balance: bool,
    /// The write-off reason being typed for a line.
    pub write_off: Option<(usize, String)>,
    /// A quantity being typed for the highlighted line (by receipt; digits and a decimal comma).
    pub typed_qty: Option<String>,
    /// Override tokens so far.
    pub tokens: Vec<String>,
    /// The posted return.
    pub result: Option<Box<SaleReturn>>,
    /// A quote is running.
    pub quoting: bool,
    /// The number of the latest quote: an older answer is ignored (it arrives after a newer one).
    pub quote_seq: u64,
    /// The attached customer when the return started.
    pub customer: Option<String>,
}

impl Return {
    /// The customer a balance refund goes to: the sale's own, or (no customer on the sale or no
    /// receipt) the attached one (§19 item 34).
    #[must_use]
    pub fn refund_customer(&self) -> Option<&str> {
        match &self.sale {
            Some(s) if s.summary.customer_id.is_some() => s.summary.customer_id.as_deref(),
            _ => self.customer.as_deref(),
        }
    }

    /// Σ refund of the picked lines.
    #[must_use]
    pub fn refund(&self) -> i64 {
        self.lines.iter().map(|l| l.refund).sum()
    }

    fn picked(&self) -> bool {
        self.lines.iter().any(|l| l.qty > 0)
    }

    /// The body to post.
    #[must_use]
    pub fn request(&self) -> ReturnRequest {
        ReturnRequest {
            sale_id: self.sale.as_ref().map(|s| s.summary.id.clone()),
            customer_id: if self.sale.is_some() {
                self.sale
                    .as_ref()
                    .and_then(|s| s.summary.customer_id.clone())
                    .or_else(|| self.customer.clone())
            } else {
                self.customer.clone()
            },
            reason: None,
            refund: if self.balance { "balance" } else { "cash" }.to_owned(),
            lines: self
                .lines
                .iter()
                .filter(|l| l.qty > 0)
                .map(|l| ReturnLineRequest {
                    sale_line_no: l.sale_line_no,
                    variant_id: l.variant_id.clone(),
                    unit_id: l.unit_id.clone(),
                    qty_thousandths: l.qty,
                    write_off_reason: l.write_off.clone(),
                })
                .collect(),
            exchange: None,
        }
    }

    /// The lines of a sale returned by receipt.
    fn of_sale(&mut self, sale: Sale) {
        self.lines = sale
            .lines
            .iter()
            .filter(|l| l.qty_thousandths > 0)
            .map(|l| ReturnLine {
                sale_line_no: Some(l.line_no),
                variant_id: l.variant_id.clone(),
                unit_id: l.unit_id.clone(),
                name: l.name.clone(),
                unit: String::new(),
                sold: l.qty_thousandths,
                left: l.qty_thousandths - l.returned_qty_thousandths,
                qty: 0,
                paid: (l.total, l.returned_qty_thousandths, l.refunded),
                refund: 0,
                write_off: None,
                // The sale line carries no unit kind: a weighed line, or one sold with decimals,
                // is a decimal unit (§19 item 68).
                decimal: l.scale_label.as_deref() == Some("weight")
                    || l.qty_thousandths % 1_000 != 0,
            })
            .collect();
        self.cursor = 0;
        self.step = ReturnStep::Lines;
        self.sale = Some(Box::new(sale));
    }

    /// The quote of the lines returned without a receipt.
    fn quote(&self) -> QuoteRequest {
        QuoteRequest {
            customer_id: self.customer.clone(),
            lines: self
                .lines
                .iter()
                .map(|l| QuoteLine::new(&l.variant_id, &l.unit_id, l.qty.max(1)))
                .collect(),
        }
    }

    /// Changes the quantity of line `i` by one unit (`up`), or to all that is left (`None`, by
    /// receipt only: without one nothing is "left"). Returns whether a quote is needed (no
    /// receipt).
    fn bump(&mut self, i: usize, up: Option<bool>) -> bool {
        let no_receipt = self.no_receipt;
        if no_receipt && up.is_none() {
            return false;
        }
        let Some(l) = self.lines.get_mut(i) else {
            return false;
        };
        l.qty = match up {
            Some(true) => (l.qty + 1_000).min(l.left),
            Some(false) => (l.qty - 1_000).max(0),
            None if l.qty == l.left => 0,
            None => l.left,
        };
        if no_receipt {
            return true;
        }
        self.refresh(i);
        false
    }

    /// Sets the typed quantity of line `i` (by receipt): whole numbers, or 3 decimals for a
    /// decimal unit (`core::sale::check_qty`), capped at what is left; `0` takes the line out.
    ///
    /// # Errors
    /// `invalid_qty` for a quantity that cannot be read or does not fit the unit.
    fn set_qty(&mut self, i: usize, text: &str) -> Result<(), SaleError> {
        let invalid = SaleError::InvalidQty { reason: "typed" };
        let qty = Qty::parse(&text.replace(',', ".")).map_err(|_| invalid.clone())?;
        let Some(l) = self.lines.get_mut(i) else {
            return Err(invalid);
        };
        if qty != Qty::ZERO {
            let kind = if l.decimal {
                UnitKind::Decimal
            } else {
                UnitKind::Integer
            };
            check_qty(qty, kind)?;
        }
        l.qty = qty.thousandths().min(l.left);
        self.refresh(i);
        Ok(())
    }

    /// Typing the quantity: digits, one decimal comma (a decimal unit), Backspace. `None` when
    /// no quantity is being typed.
    fn type_qty(&mut self, key: Key) -> Option<bool> {
        let decimal = self.lines.get(self.cursor).is_some_and(|l| l.decimal);
        let t = self.typed_qty.as_mut()?;
        Some(match key {
            Key::Digit(d) if t.len() < 9 => {
                t.push(char::from(b'0' + d));
                true
            }
            Key::Char(',' | '.') if decimal && !t.contains(',') => {
                t.push(',');
                true
            }
            Key::Backspace => {
                t.pop();
                true
            }
            _ => false,
        })
    }

    /// The refund of line `i`'s quantity (`core::sale::line_refund`, by receipt).
    fn refresh(&mut self, i: usize) {
        let Some(l) = self.lines.get_mut(i) else {
            return;
        };
        let (total, returned, refunded) = l.paid;
        l.refund = line_refund(
            SoldLine {
                qty: Qty::from_thousandths(l.sold),
                amount: Money::from_minor(total),
            },
            ReturnedSoFar {
                qty: Qty::from_thousandths(returned),
                refunded: Money::from_minor(refunded),
            },
            Qty::from_thousandths(l.qty),
        )
        .map_or(0, Money::minor);
    }
}

/// What an approved override retries in this module.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Waiting {
    /// The drawer open.
    Drawer(String),
    /// Dropping a held cart.
    DropHeld(String),
    /// A reprint of this sale.
    Reprint(String),
    /// A post-void.
    PostVoid {
        /// The sale.
        id: String,
        /// The body.
        body: VoidRequest,
        /// Idempotency key.
        key: String,
    },
    /// The open return.
    Return,
}

/// A held cart's payload: the crash copy plus the names the list shows.
fn payload(saved: &Saved, register: &str, cashier: &str) -> String {
    json!({ "cart": saved, "register": register, "cashier": cashier }).to_string()
}

fn held_row(c: &HeldCart, cashier: Option<&str>) -> HeldRow {
    let p = &c.payload;
    let text = |v: &serde_json::Value| v.as_str().unwrap_or_default().to_owned();
    HeldRow {
        id: c.id.clone(),
        register: Some(text(&p["register"]))
            .filter(|r| !r.is_empty())
            .unwrap_or_else(|| c.register_id.clone()),
        cashier: Some(text(&p["cashier"]))
            .filter(|r| !r.is_empty())
            .or_else(|| cashier.map(str::to_owned))
            .unwrap_or_else(|| c.user_id.clone()),
        at: c.held_at,
        customer: text(&p["cart"]["customer"]["name"]),
        items: c.item_count,
        total: c.total,
        note: c.note.clone().unwrap_or_default(),
    }
}

fn doc_of(s: &SaleSummary) -> Doc {
    Doc {
        kind: "sale".to_owned(),
        id: s.id.clone(),
        number: s.number.clone(),
        at: s.posted_at,
        total: s.paid,
        customer: s.customer_id.is_some(),
        voided: s.status == "voided",
        branch: s.branch_id.clone(),
    }
}

/// The message for a failed call of this module (spec 2.6 error codes).
#[must_use]
pub fn after_notice(error: &ClientError) -> Notice {
    match error.code() {
        Some("cart_taken") => Notice::bad("cart_taken"),
        Some("has_returns") => Notice::bad("has_returns"),
        Some("return_qty_exceeded") => Notice::bad("return_qty_exceeded"),
        Some("return_window") => Notice::bad("return_window"),
        Some("conflict") => Notice::bad("already_voided"),
        Some("not_found") => Notice::bad("receipt_not_found"),
        Some("invalid_field") => match error.problem().and_then(|p| p.extra.get("field")) {
            Some(f) if f == "sale" => Notice::bad("other_branch_sale"),
            Some(f) if f == "customer" => Notice::bad("needs_customer"),
            _ => notice_for(error),
        },
        _ => notice_for(error),
    }
}

impl Controller {
    fn home_mut(&mut self) -> Option<&mut crate::controller::HomeScreen> {
        match &mut self.screen {
            Screen::Home(h) => Some(h),
            _ => None,
        }
    }

    fn modal(&self) -> Option<&Modal> {
        self.sell().and_then(|s| s.modal.as_ref())
    }

    fn modal_mut(&mut self) -> Option<&mut Modal> {
        self.home_mut().and_then(|h| h.sell.modal.as_mut())
    }

    fn call(&mut self, call: Call) -> Vec<Effect> {
        self.busy = true;
        vec![Effect::After {
            token: self.token(),
            call,
            generation: self.generation,
        }]
    }

    /// A call that does not block input (lists and quotes).
    fn quiet(&self, call: Call) -> Vec<Effect> {
        vec![Effect::After {
            token: self.token(),
            call,
            generation: self.generation,
        }]
    }

    /// Whether one of this module's dialogs is open.
    pub(crate) fn in_after(&self) -> bool {
        matches!(
            self.modal(),
            Some(
                Modal::Note(_)
                    | Modal::Held(_)
                    | Modal::Docs(_)
                    | Modal::Receipt(_)
                    | Modal::PostVoid(_)
                    | Modal::Return(_)
            )
        )
    }

    /// The keys of this module's dialogs.
    #[allow(clippy::too_many_lines)] // one list per dialog and step
    pub(crate) fn after_shortcuts(&self) -> Option<Vec<Shortcut>> {
        use Action::{Back, Confirm};
        let s = |key, action, label| Shortcut { key, action, label };
        Some(match self.modal()? {
            Modal::Note(_) => vec![
                s(Key::Enter, Confirm, "hold"),
                s(Key::Escape, Back, "cancel"),
            ],
            Modal::Held(list) => {
                let mut keys = Vec::new();
                if !list.rows.is_empty() {
                    keys.push(s(Key::Enter, Confirm, "resume"));
                    keys.push(s(Key::Delete, Action::DeleteHeld, "delete_held"));
                }
                keys.push(s(Key::Escape, Back, "close"));
                keys
            }
            Modal::Docs(d) => {
                let mut keys = vec![s(Key::Enter, Confirm, "show_receipt")];
                if d.next.is_some() {
                    keys.push(s(Key::F(8), Action::MorePage, "more_page"));
                }
                keys.push(s(Key::Escape, Back, "close"));
                keys
            }
            Modal::Receipt(p) => {
                let mut keys = Vec::new();
                if let Some(sale) = &p.sale {
                    keys.push(s(Key::Char('r'), Action::Reprint, "reprint"));
                    if sale.summary.status == "posted" {
                        keys.push(s(Key::Char('v'), Action::PostVoid, "post_void"));
                        keys.push(s(Key::Char('t'), Action::StartReturn, "start_return"));
                    }
                }
                keys.push(s(Key::Escape, Back, "back"));
                keys
            }
            Modal::PostVoid(v) => {
                let mut keys = vec![s(Key::Enter, Confirm, "post_void")];
                if v.sale.summary.customer_id.is_some() {
                    keys.push(s(Key::Left, Action::RefundMethod, "refund_method"));
                    keys.push(s(Key::Right, Action::RefundMethod, "refund_method"));
                }
                keys.push(s(Key::Escape, Back, "cancel"));
                keys
            }
            Modal::Return(r) => match r.step {
                _ if r.write_off.is_some() || r.typed_qty.is_some() => {
                    vec![s(Key::Enter, Confirm, "ok"), s(Key::Escape, Back, "cancel")]
                }
                ReturnStep::Find => vec![
                    s(Key::Enter, Confirm, "find"),
                    s(Key::F(2), Action::NoReceipt, "no_receipt"),
                    s(Key::Escape, Back, "close"),
                ],
                ReturnStep::Lines => {
                    let mut keys = Vec::new();
                    if r.no_receipt && !r.text.is_empty() {
                        keys.push(s(Key::Enter, Confirm, "add_item"));
                    } else if r.picked() {
                        keys.push(s(Key::Enter, Confirm, "refund"));
                        if self.sell().is_none_or(|s| s.exchange.is_none()) {
                            keys.push(s(Key::F(9), Action::Exchange, "exchange"));
                        }
                    }
                    if !r.lines.is_empty() && r.text.is_empty() {
                        if !r.no_receipt {
                            keys.push(s(Key::Char('q'), Action::EditQty, "edit_qty"));
                        }
                        keys.push(s(Key::Char('+'), Action::More, "more"));
                        keys.push(s(Key::Char('-'), Action::Less, "less"));
                        if !r.no_receipt {
                            keys.push(s(Key::Char('*'), Action::All, "return_all"));
                        }
                        keys.push(s(Key::Delete, Action::WriteOff, "write_off"));
                    }
                    if r.refund_customer().is_some() {
                        keys.push(s(Key::Tab, Action::RefundMethod, "refund_method"));
                    }
                    keys.push(s(Key::Escape, Back, "back"));
                    keys
                }
                ReturnStep::Done => vec![s(Key::Enter, Confirm, "done")],
            },
            _ => return None,
        })
    }

    /// Typing in this module's dialogs. `None` when none is open.
    pub(crate) fn after_edit(&mut self, key: Key) -> Option<bool> {
        let typed = match key {
            Key::Digit(d) => Some(char::from(b'0' + d)),
            Key::Char(c) => Some(c),
            _ => None,
        };
        let text = |t: &mut String, max: usize| match (key, typed) {
            (_, Some(c)) if t.chars().count() < max => {
                t.push(c);
                true
            }
            (Key::Backspace, _) => {
                t.pop();
                true
            }
            _ => false,
        };
        let wrap = crate::count::wrap;
        Some(match self.modal_mut()? {
            Modal::Note(n) => text(&mut n.text, 200),
            Modal::Held(l) => match key {
                Key::Up => {
                    l.cursor = wrap(l.cursor, -1, l.rows.len());
                    true
                }
                Key::Down => {
                    l.cursor = wrap(l.cursor, 1, l.rows.len());
                    true
                }
                _ => false,
            },
            Modal::Docs(d) => match key {
                Key::Up => {
                    d.cursor = wrap(d.cursor, -1, d.rows.len());
                    true
                }
                Key::Down => {
                    d.cursor = wrap(d.cursor, 1, d.rows.len());
                    true
                }
                _ if d.history.is_none() => text(&mut d.number, 40),
                _ => false,
            },
            Modal::PostVoid(v) => match key {
                Key::Up => {
                    v.cursor = wrap(v.cursor, -1, VOID_REASONS.len());
                    true
                }
                Key::Down => {
                    v.cursor = wrap(v.cursor, 1, VOID_REASONS.len());
                    true
                }
                _ if v.cursor == 3 => text(&mut v.text, 200),
                _ => false,
            },
            Modal::Return(r) => {
                if let Some((_, reason)) = &mut r.write_off {
                    return Some(text(reason, 200));
                }
                if let Some(used) = r.type_qty(key) {
                    return Some(used);
                }
                let count = if r.step == ReturnStep::Find {
                    r.recent.len()
                } else {
                    r.lines.len()
                };
                match (r.step, key) {
                    (ReturnStep::Done, _) => false,
                    (_, Key::Up) => {
                        r.cursor = wrap(r.cursor, -1, count);
                        true
                    }
                    (_, Key::Down) => {
                        r.cursor = wrap(r.cursor, 1, count);
                        true
                    }
                    (ReturnStep::Find, _) => text(&mut r.text, 40),
                    (ReturnStep::Lines, Key::Digit(d)) if !r.no_receipt && !r.lines.is_empty() => {
                        r.typed_qty = Some(char::from(b'0' + d).to_string());
                        true
                    }
                    (ReturnStep::Lines, Key::Char('+' | '-' | '*')) if r.text.is_empty() => false,
                    (ReturnStep::Lines, _) if r.no_receipt => text(&mut r.text, 40),
                    _ => false,
                }
            }
            _ => return None,
        })
    }

    /// A click on a row of this module's lists.
    pub(crate) fn after_pick(&mut self, index: usize) {
        match self.modal_mut() {
            Some(Modal::Held(l)) if index < l.rows.len() => l.cursor = index,
            Some(Modal::Docs(d)) if index < d.rows.len() => d.cursor = index,
            Some(Modal::PostVoid(v)) if index < VOID_REASONS.len() => v.cursor = index,
            Some(Modal::Return(r)) if r.step == ReturnStep::Find && index < r.recent.len() => {
                r.cursor = index;
            }
            Some(Modal::Return(r)) if r.step == ReturnStep::Lines && index < r.lines.len() => {
                r.cursor = index;
                r.typed_qty = None;
            }
            _ => {}
        }
    }

    /// The home keys of this module (F5, F6, F10, F11, Shift+F11).
    #[allow(clippy::too_many_lines)] // one arm per action of the module
    pub(crate) fn after_action(&mut self, action: Action) -> Vec<Effect> {
        match action {
            Action::Drawer => {
                let key = new_key();
                self.set_waiting(Waiting::Drawer(key.clone()));
                self.call(Call::Drawer {
                    key,
                    override_token: None,
                })
            }
            Action::Hold => {
                if self.sell().is_some_and(|s| !s.cart.is_empty()) {
                    self.open(Modal::Note(HoldNote::default()));
                }
                Vec::new()
            }
            Action::Resume => {
                if self.sell().is_some_and(|s| !s.cart.is_empty()) {
                    self.open(Modal::Note(HoldNote {
                        text: String::new(),
                        then_resume: true,
                    }));
                    self.set_notice(Notice::new("hold_first", "", Tone::Neutral));
                    return Vec::new();
                }
                self.open(Modal::Held(HeldList::default()));
                self.call(Call::HeldCarts)
            }
            Action::Receipts => {
                let Some(session) = self.session.as_ref().map(|s| s.id.clone()) else {
                    return Vec::new();
                };
                self.open(Modal::Docs(Docs::default()));
                self.call(Call::SessionSales(session))
            }
            Action::Return => {
                if self.sell().is_some_and(|s| s.exchange.is_some()) {
                    if let Some(h) = self.home_mut() {
                        h.sell.exchange = None;
                        h.notice = Some(Notice::new("exchange_cancelled", "", Tone::Neutral));
                    }
                    return Vec::new();
                }
                self.start_return(None)
            }
            Action::DeleteHeld => {
                let Some(Modal::Held(l)) = self.modal() else {
                    return Vec::new();
                };
                let Some(id) = l.rows.get(l.cursor).map(|r| r.id.clone()) else {
                    return Vec::new();
                };
                self.set_waiting(Waiting::DropHeld(id.clone()));
                self.call(Call::DropHeld {
                    id,
                    override_token: None,
                })
            }
            Action::MorePage => {
                let Some(Modal::Docs(d)) = self.modal() else {
                    return Vec::new();
                };
                match (&d.history, &d.next) {
                    (Some((customer, _)), Some(next)) => {
                        let call = Call::History {
                            customer_id: customer.clone(),
                            before: Some(next.clone()),
                        };
                        self.call(call)
                    }
                    _ => Vec::new(),
                }
            }
            Action::Reprint => {
                let Some(Modal::Receipt(p)) = self.modal() else {
                    return Vec::new();
                };
                let id = p.id.clone();
                self.set_waiting(Waiting::Reprint(id.clone()));
                self.call(Call::Receipt {
                    id,
                    copy: true,
                    override_token: None,
                })
            }
            Action::PostVoid => {
                let Some(Modal::Receipt(p)) = self.modal().cloned() else {
                    return Vec::new();
                };
                if let Some(sale) = p.sale {
                    self.open(Modal::PostVoid(Box::new(PostVoid {
                        sale,
                        cursor: 0,
                        text: String::new(),
                        balance: false,
                        key: new_key(),
                        back: p.back,
                    })));
                }
                Vec::new()
            }
            Action::StartReturn => {
                let Some(Modal::Receipt(p)) = self.modal().cloned() else {
                    return Vec::new();
                };
                match p.sale {
                    Some(sale) => self.start_return(Some(*sale)),
                    None => Vec::new(),
                }
            }
            Action::RefundMethod => {
                match self.modal_mut() {
                    Some(Modal::PostVoid(v)) => v.balance = !v.balance,
                    Some(Modal::Return(r)) => r.balance = !r.balance,
                    _ => {}
                }
                Vec::new()
            }
            Action::NoReceipt => {
                if let Some(Modal::Return(r)) = self.modal_mut() {
                    r.no_receipt = true;
                    r.step = ReturnStep::Lines;
                    r.text.clear();
                    r.cursor = 0;
                }
                Vec::new()
            }
            Action::More | Action::Less | Action::All => {
                let up = match action {
                    Action::More => Some(true),
                    Action::Less => Some(false),
                    _ => None,
                };
                let quote = match self.modal_mut() {
                    Some(Modal::Return(r)) => {
                        let i = r.cursor;
                        r.bump(i, up).then(|| r.quote())
                    }
                    _ => None,
                };
                self.return_quote(quote)
            }
            Action::WriteOff => {
                if let Some(Modal::Return(r)) = self.modal_mut() {
                    let i = r.cursor;
                    if let Some(l) = r.lines.get_mut(i)
                        && l.write_off.take().is_none()
                    {
                        r.write_off = Some((i, String::new()));
                    }
                }
                Vec::new()
            }
            Action::EditQty => {
                if let Some(Modal::Return(r)) = self.modal_mut()
                    && r.step == ReturnStep::Lines
                    && !r.no_receipt
                    && !r.lines.is_empty()
                {
                    r.typed_qty = Some(String::new());
                }
                Vec::new()
            }
            Action::Exchange => self.start_exchange(),
            Action::Confirm => self.after_confirm(),
            Action::Back => {
                self.after_back();
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    fn open(&mut self, modal: Modal) {
        if let Some(h) = self.home_mut() {
            h.sell.modal = Some(modal);
            h.sell.focus = None;
        }
    }

    fn set_waiting(&mut self, waiting: Waiting) {
        if let Some(h) = self.home_mut() {
            h.sell.waiting = Some(waiting);
        }
    }

    /// Whether quote `seq` is the open return's latest (a stale answer, or one for a closed
    /// dialog, is dropped).
    fn latest_quote(&self, seq: u64) -> bool {
        matches!(self.modal(), Some(Modal::Return(r)) if r.quote_seq == seq)
    }

    fn return_quote(&mut self, quote: Option<QuoteRequest>) -> Vec<Effect> {
        let Some(request) = quote else {
            return Vec::new();
        };
        if request.lines.is_empty() {
            return Vec::new();
        }
        let Some(Modal::Return(r)) = self.modal_mut() else {
            return Vec::new();
        };
        r.quoting = true;
        r.quote_seq += 1;
        let seq = r.quote_seq;
        self.quiet(Call::Quote(seq, request))
    }

    /// F10 or T: a return of `sale`, or the Find step (an empty cart only).
    fn start_return(&mut self, sale: Option<Sale>) -> Vec<Effect> {
        if self.sell().is_some_and(|s| !s.cart.is_empty()) {
            self.set_notice(Notice::bad("cart_not_empty"));
            if let Some(h) = self.home_mut() {
                h.sell.modal = None;
            }
            return Vec::new();
        }
        let customer = self.customer.as_ref().map(|c| c.id.clone());
        let mut r = Return {
            id: new_key(),
            customer: customer.clone(),
            ..Return::default()
        };
        if let Some(sale) = sale {
            r.of_sale(sale);
            self.open(Modal::Return(Box::new(r)));
            return Vec::new();
        }
        self.open(Modal::Return(Box::new(r)));
        match customer {
            Some(c) => self.quiet(Call::Recent(c)),
            None => Vec::new(),
        }
    }

    /// F9 in the return: the refund becomes the credit of a new cart (D12).
    fn start_exchange(&mut self) -> Vec<Effect> {
        let Some(h) = self.home_mut() else {
            return Vec::new();
        };
        let Some(Modal::Return(r)) = h.sell.modal.take() else {
            return Vec::new();
        };
        let credit = r.refund();
        h.sell.exchange = Some(r);
        h.notice = Some(Notice::new("exchange_started", rupiah(credit), Tone::Good));
        Vec::new()
    }

    fn after_back(&mut self) {
        let Some(h) = self.home_mut() else {
            return;
        };
        let Some(modal) = h.sell.modal.take() else {
            return;
        };
        h.sell.modal = match modal {
            Modal::Receipt(p) => p.back.map(|d| Modal::Docs(*d)),
            Modal::PostVoid(v) => v.back.map(|d| Modal::Docs(*d)),
            Modal::Return(mut r) if r.write_off.is_some() || r.typed_qty.is_some() => {
                r.write_off = None;
                r.typed_qty = None;
                Some(Modal::Return(r))
            }
            Modal::Return(mut r) if r.step == ReturnStep::Lines && !r.text.is_empty() => {
                r.text.clear();
                Some(Modal::Return(r))
            }
            Modal::Return(mut r) if r.step == ReturnStep::Lines => {
                r.step = ReturnStep::Find;
                r.lines.clear();
                r.sale = None;
                r.no_receipt = false;
                r.cursor = 0;
                Some(Modal::Return(r))
            }
            _ => None,
        };
    }

    #[allow(clippy::too_many_lines)] // one arm per dialog
    fn after_confirm(&mut self) -> Vec<Effect> {
        let Some(modal) = self.modal().cloned() else {
            return Vec::new();
        };
        match modal {
            Modal::Note(n) => self.hold(&n),
            Modal::Held(l) => match l.rows.get(l.cursor) {
                Some(row) => {
                    let id = row.id.clone();
                    self.call(Call::Take { id, key: new_key() })
                }
                None => Vec::new(),
            },
            Modal::Docs(d) => {
                let number = d.number.trim().to_owned();
                if !number.is_empty() {
                    return self.call(Call::SaleByNumber(number));
                }
                match d.rows.get(d.cursor) {
                    Some(doc) if doc.kind == "sale" => {
                        let id = doc.id.clone();
                        self.show_receipt(id, Some(Box::new(d)))
                    }
                    Some(_) => {
                        self.set_notice(Notice::bad("return_no_receipt"));
                        Vec::new()
                    }
                    None => Vec::new(),
                }
            }
            Modal::PostVoid(v) => {
                let reason = VOID_REASONS[v.cursor.min(VOID_REASONS.len() - 1)];
                let text = v.text.trim().to_owned();
                if reason == "other" && text.is_empty() {
                    self.set_notice(Notice::bad("reason_text_required"));
                    return Vec::new();
                }
                let body = VoidRequest {
                    reason: reason.to_owned(),
                    reason_text: (reason == "other").then_some(text),
                    refund: if v.balance { "balance" } else { "cash" }.to_owned(),
                };
                let id = v.sale.summary.id.clone();
                self.set_waiting(Waiting::PostVoid {
                    id: id.clone(),
                    body: body.clone(),
                    key: v.key.clone(),
                });
                self.call(Call::Void {
                    id,
                    body,
                    key: v.key,
                    override_token: None,
                })
            }
            Modal::Return(r) => self.return_confirm(&r),
            _ => Vec::new(),
        }
    }

    fn return_confirm(&mut self, r: &Return) -> Vec<Effect> {
        if let Some((i, reason)) = &r.write_off {
            let reason = reason.trim().to_owned();
            if reason.is_empty() {
                self.set_notice(Notice::bad("reason_text_required"));
                return Vec::new();
            }
            let i = *i;
            if let Some(Modal::Return(r)) = self.modal_mut() {
                if let Some(l) = r.lines.get_mut(i) {
                    l.write_off = Some(reason);
                }
                r.write_off = None;
            }
            return Vec::new();
        }
        if let Some(typed) = &r.typed_qty {
            let i = r.cursor;
            let typed = typed.clone();
            let result = match self.modal_mut() {
                Some(Modal::Return(r)) => r.set_qty(i, &typed),
                _ => Ok(()),
            };
            match result {
                Ok(()) => {
                    if let Some(Modal::Return(r)) = self.modal_mut() {
                        r.typed_qty = None;
                    }
                }
                Err(e) => self.set_notice(crate::till::sale_notice(&e)),
            }
            return Vec::new();
        }
        match r.step {
            ReturnStep::Done => {
                if let Some(h) = self.home_mut() {
                    h.sell.modal = None;
                }
                Vec::new()
            }
            ReturnStep::Find => {
                let number = r.text.trim().to_owned();
                if !number.is_empty() {
                    return self.call(Call::SaleByNumber(number));
                }
                match r.recent.get(r.cursor) {
                    Some(s) => {
                        let id = s.id.clone();
                        self.call(Call::Sale(id))
                    }
                    None => Vec::new(),
                }
            }
            ReturnStep::Lines if r.no_receipt && !r.text.is_empty() => {
                let code = r.text.trim().to_owned();
                if let Some(Modal::Return(r)) = self.modal_mut() {
                    r.text.clear();
                }
                self.call(Call::Lookup(code))
            }
            ReturnStep::Lines if r.picked() && !r.quoting => {
                self.set_waiting(Waiting::Return);
                let override_token = Some(r.tokens.join(",")).filter(|t| !t.is_empty());
                self.call(Call::Return {
                    id: r.id.clone(),
                    body: Box::new(r.request()),
                    override_token,
                })
            }
            ReturnStep::Lines => Vec::new(),
        }
    }

    /// F11: holds the cart (`PUT /held-carts/{id}`).
    fn hold(&mut self, note: &HoldNote) -> Vec<Effect> {
        let Some(session) = self.session.as_ref().map(|s| s.id.clone()) else {
            return Vec::new();
        };
        let cashier = self
            .login
            .as_ref()
            .map(|l| l.display_name.clone())
            .unwrap_or_default();
        let register = self.register_label.clone();
        let customer = self.customer.clone();
        let Some(state) = self.sell() else {
            return Vec::new();
        };
        let saved = Saved::of(&state.cart, &session, customer.as_ref());
        let total = state.cart.totals().map_or(0, |t| t.total.minor());
        let items = u32::try_from(state.cart.item_count()).unwrap_or(u32::MAX);
        let text = note.text.trim();
        let call = Call::Hold {
            id: state.cart.id.clone(),
            customer_id: customer.map(|c| c.id),
            note: (!text.is_empty()).then(|| text.to_owned()),
            payload: payload(&saved, &register, &cashier),
            item_count: items,
            total,
        };
        self.call(call)
    }

    fn show_receipt(&mut self, id: String, back: Option<Box<Docs>>) -> Vec<Effect> {
        self.open(Modal::Receipt(Box::new(Preview {
            id: id.clone(),
            back,
            ..Preview::default()
        })));
        let mut effects = self.call(Call::Sale(id.clone()));
        effects.extend(self.quiet(Call::Receipt {
            id,
            copy: false,
            override_token: None,
        }));
        effects
    }

    /// Opens a customer's purchase history (from the customer dialog).
    pub(crate) fn open_history(&mut self, customer: &Customer) -> Vec<Effect> {
        self.open(Modal::Docs(Docs {
            history: Some((customer.id.clone(), customer.name.clone())),
            ..Docs::default()
        }));
        self.call(Call::History {
            customer_id: customer.id.clone(),
            before: None,
        })
    }

    /// `cart.held` / `cart.taken`: an open resume list loads again.
    pub(crate) fn held_event(&mut self, kind: &str) -> Vec<Effect> {
        if matches!(kind, "cart.held" | "cart.taken")
            && matches!(self.modal(), Some(Modal::Held(_)))
        {
            return self.quiet(Call::HeldCarts);
        }
        Vec::new()
    }

    /// A manager approved: the waiting call again with the token.
    pub(crate) fn after_retry(&mut self, token: String) -> Vec<Effect> {
        let Some(waiting) = self.sell().and_then(|s| s.waiting.clone()) else {
            self.busy = false;
            return Vec::new();
        };
        let t = Some(token.clone());
        match waiting {
            Waiting::Drawer(key) => self.call(Call::Drawer {
                key,
                override_token: t,
            }),
            Waiting::DropHeld(id) => self.call(Call::DropHeld {
                id,
                override_token: t,
            }),
            Waiting::Reprint(id) => self.call(Call::Receipt {
                id,
                copy: true,
                override_token: t,
            }),
            Waiting::PostVoid { id, body, key } => self.call(Call::Void {
                id,
                body,
                key,
                override_token: t,
            }),
            Waiting::Return => {
                if let Some(Modal::Return(r)) = self.modal_mut() {
                    r.tokens.push(token);
                }
                self.after_confirm()
            }
        }
    }

    /// The answer to a call of this module.
    #[allow(clippy::too_many_lines)] // one arm per answer
    pub(crate) fn after_reply(&mut self, generation: u64, reply: Reply) -> Vec<Effect> {
        if generation != self.generation || self.login.is_none() {
            return Vec::new();
        }
        if let Some(e) = reply.error().filter(|e| ends_login(e)).cloned() {
            return self.fail(&e);
        }
        if let Reply::Exchanged(result) = reply {
            return self.exchanged(generation, result);
        }
        if !matches!(reply, Reply::Quoted(..) | Reply::Recent(_)) {
            self.busy = false;
        }
        if let Some(e) = reply.error()
            && matches!(e.code(), Some("needs_override" | "return_window"))
            && self.approval.is_none()
            && self.sell().is_some_and(|s| s.waiting.is_some())
        {
            let e = e.clone();
            return self.after_needs_override(&e);
        }
        match reply {
            Reply::Held(Ok(_)) => {
                let then_resume = matches!(
                    self.modal(),
                    Some(Modal::Note(HoldNote {
                        then_resume: true,
                        ..
                    }))
                );
                self.customer = None;
                if let Some(h) = self.home_mut() {
                    h.sell = crate::till::Sell::default();
                    h.notice = Some(Notice::new("cart_held", "", Tone::Good));
                }
                let mut effects = vec![Effect::DeleteCart];
                if then_resume {
                    self.open(Modal::Held(HeldList::default()));
                    effects.extend(self.call(Call::HeldCarts));
                }
                effects
            }
            Reply::HeldCarts(Ok(carts)) => {
                let rows: Vec<HeldRow> = carts
                    .iter()
                    .map(|c| held_row(c, self.user_name(&c.user_id)))
                    .collect();
                if let Some(Modal::Held(l)) = self.modal_mut() {
                    l.cursor = l.cursor.min(rows.len().saturating_sub(1));
                    l.rows = rows;
                }
                Vec::new()
            }
            Reply::Taken(Ok(cart)) => self.resumed(&cart),
            Reply::Taken(Err(e)) if e.code() == Some("cart_taken") => {
                self.set_notice(Notice::bad("cart_taken"));
                self.call(Call::HeldCarts)
            }
            Reply::Dropped(Ok(_)) => {
                self.approval = None;
                self.retry = None;
                if let Some(h) = self.home_mut() {
                    h.sell.waiting = None;
                }
                self.set_notice(Notice::new("held_deleted", "", Tone::Neutral));
                self.call(Call::HeldCarts)
            }
            Reply::Sales(Ok(page)) => {
                if let Some(Modal::Docs(d)) = self.modal_mut() {
                    d.rows = page.items.iter().map(doc_of).collect();
                    d.cursor = 0;
                }
                Vec::new()
            }
            Reply::History(Ok(page)) => {
                if let Some(Modal::Docs(d)) = self.modal_mut() {
                    let more = page.items.iter().map(|i| Doc {
                        kind: i.kind.clone(),
                        id: i.document_id.clone(),
                        number: i.number.clone(),
                        at: i.posted_at,
                        total: i.total,
                        customer: true,
                        voided: i.voided,
                        branch: i.branch_id.clone(),
                    });
                    d.rows.extend(more);
                    d.next = page.next_before_id;
                }
                Vec::new()
            }
            Reply::Recent(Ok(recent)) => {
                if let Some(Modal::Return(r)) = self.modal_mut() {
                    r.recent = recent;
                }
                Vec::new()
            }
            Reply::Sale(Ok(sale)) => self.sale_loaded(sale),
            Reply::Receipt(Ok(receipt)) => {
                let reprint = self.sell().and_then(|s| s.waiting.clone());
                if matches!(reprint, Some(Waiting::Reprint(_))) {
                    self.approval = None;
                    self.retry = None;
                    if let Some(h) = self.home_mut() {
                        h.sell.waiting = None;
                    }
                    self.set_notice(Notice::new("reprinted", receipt.number.clone(), Tone::Good));
                }
                if let Some(Modal::Receipt(p)) = self.modal_mut()
                    && p.id == receipt.sale_id
                {
                    p.lines = receipt.lines;
                }
                Vec::new()
            }
            Reply::Voided(Ok(sale)) => {
                self.approval = None;
                self.retry = None;
                let (cash, balance) = sale
                    .void
                    .as_ref()
                    .map_or((0, 0), |v| (v.cash, v.to_balance + v.payment_reversed));
                let back = match self.modal() {
                    Some(Modal::PostVoid(v)) => v.back.clone(),
                    _ => None,
                };
                if let Some(h) = self.home_mut() {
                    h.sell.waiting = None;
                    h.notice = Some(Notice::new(
                        "sale_voided",
                        sale.summary.number.clone(),
                        Tone::Good,
                    ));
                }
                let id = sale.summary.id.clone();
                self.open(Modal::Receipt(Box::new(Preview {
                    sale: Some(Box::new(sale)),
                    id: id.clone(),
                    lines: Vec::new(),
                    back: back.map(|mut d| {
                        if let Some(doc) = d.rows.iter_mut().find(|x| x.id == id) {
                            doc.voided = true;
                        }
                        d
                    }),
                    refund: Some((cash, balance)),
                })));
                self.quiet(Call::Receipt {
                    id,
                    copy: false,
                    override_token: None,
                })
            }
            Reply::Returned(Ok(posted)) => {
                self.approval = None;
                self.retry = None;
                if let Some(h) = self.home_mut() {
                    h.sell.waiting = None;
                }
                if let Some(Modal::Return(r)) = self.modal_mut() {
                    r.step = ReturnStep::Done;
                    r.result = Some(Box::new(posted));
                }
                Vec::new()
            }
            Reply::Looked(Ok(item)) => {
                let quote = match self.modal_mut() {
                    Some(Modal::Return(r)) => {
                        let found = r.lines.iter().position(|l| {
                            l.variant_id == item.variant_id && l.unit_id == item.unit_id
                        });
                        let i = found.unwrap_or_else(|| {
                            r.lines.push(ReturnLine {
                                variant_id: item.variant_id.clone(),
                                unit_id: item.unit_id.clone(),
                                name: item.name.clone(),
                                unit: item.unit_name.clone(),
                                left: NO_RECEIPT_LEFT,
                                ..ReturnLine::default()
                            });
                            r.lines.len() - 1
                        });
                        let step = item.qty_thousandths.unwrap_or(1_000);
                        r.lines[i].qty += step;
                        r.cursor = i;
                        Some(r.quote())
                    }
                    _ => None,
                };
                self.return_quote(quote)
            }
            Reply::Quoted(seq, _) if !self.latest_quote(seq) => Vec::new(),
            Reply::Quoted(_, Ok(quote)) => {
                if let Some(Modal::Return(r)) = self.modal_mut() {
                    r.quoting = false;
                    for (l, q) in r.lines.iter_mut().zip(&quote.lines) {
                        let total = q.tax.as_ref().map_or(0, |t| t.total);
                        l.refund = if l.qty > 0 { total } else { 0 };
                    }
                }
                Vec::new()
            }
            Reply::Drawer(Ok(_)) => {
                self.approval = None;
                self.retry = None;
                if let Some(h) = self.home_mut() {
                    h.sell.waiting = None;
                }
                self.set_notice(Notice::new("drawer_opened", "", Tone::Neutral));
                Vec::new()
            }
            Reply::Quoted(_, Err(e)) => {
                if let Some(Modal::Return(r)) = self.modal_mut() {
                    r.quoting = false;
                }
                self.set_notice(crate::controller::quote_notice(&e));
                Vec::new()
            }
            Reply::Looked(Err(e)) => {
                self.set_notice(crate::controller::lookup_notice(&e, ""));
                Vec::new()
            }
            other => {
                let Some(e) = other.error().cloned() else {
                    return Vec::new();
                };
                let notice = after_notice(&e);
                // A refused token stays in the dialog; any other answer closes it.
                let token_error = e.code() == Some("invalid_override");
                if let Some(a) = self.approval.as_mut().filter(|_| token_error) {
                    a.notice = Some(notice);
                } else {
                    self.approval = None;
                    self.retry = None;
                    if let Some(h) = self.home_mut() {
                        h.sell.waiting = None;
                    }
                    self.set_notice(notice);
                }
                Vec::new()
            }
        }
    }

    /// The answer of an exchange: the new sale as a posted sale, plus the cash paid back.
    fn exchanged(
        &mut self,
        generation: u64,
        result: Result<SaleReturn, ClientError>,
    ) -> Vec<Effect> {
        match result {
            Ok(posted) => {
                let cash = posted
                    .tenders
                    .iter()
                    .filter(|t| t.kind == "cash")
                    .map(|t| t.amount)
                    .sum();
                if let Some(sale) = posted.exchange_sale {
                    self.posted_with(generation, Ok(sale), cash)
                } else {
                    self.busy = false;
                    Vec::new()
                }
            }
            Err(e) => self.posted_with(generation, Err(e), 0),
        }
    }

    /// `403 needs_override` / `return_window` for the waiting call: the override dialog.
    fn after_needs_override(&mut self, e: &ClientError) -> Vec<Effect> {
        let problem = e.problem();
        let permission = problem
            .and_then(sultan_client::Problem::permission)
            .unwrap_or_default()
            .to_owned();
        let limit = problem.and_then(sultan_client::Problem::limit);
        let session = self
            .session
            .as_ref()
            .map(|s| s.id.clone())
            .unwrap_or_default();
        let (context, value) = match self.sell().and_then(|s| s.waiting.clone()) {
            Some(Waiting::Drawer(_)) => (session, None),
            Some(Waiting::DropHeld(id) | Waiting::Reprint(id) | Waiting::PostVoid { id, .. }) => {
                (id, None)
            }
            Some(Waiting::Return) => match self.modal() {
                Some(Modal::Return(r)) => (
                    r.id.clone(),
                    (permission == "refund.no_receipt").then(|| r.refund()),
                ),
                _ => return Vec::new(),
            },
            None => return Vec::new(),
        };
        self.approval = Some(crate::controller::Approval {
            action: ProtectedAction {
                action: permission,
                value,
                context_ref: context,
            },
            limit,
            cursor: 0,
            pin: crate::input::Pin::default(),
            notice: None,
        });
        self.retry = Some(Retry::After);
        Vec::new()
    }

    /// A taken held cart becomes the cart (under a new cart ID), quoted again.
    fn resumed(&mut self, cart: &HeldCart) -> Vec<Effect> {
        let saved = serde_json::from_value::<Saved>(cart.payload["cart"].clone()).ok();
        let Some(saved) = saved else {
            self.set_notice(Notice::bad("held_unreadable"));
            return Vec::new();
        };
        let mut restored = saved.cart();
        restored.id = new_key();
        let count = restored.lines.len();
        if saved.customer.is_some() {
            self.customer.clone_from(&saved.customer);
        }
        if let Some(h) = self.home_mut() {
            h.sell.cart = restored;
            h.sell.modal = None;
            h.notice = Some(Notice::new("cart_resumed", count.to_string(), Tone::Good));
        }
        self.cart_changed()
    }

    /// A sale found by number or ID: the receipt preview, or the return's lines.
    fn sale_loaded(&mut self, sale: Sale) -> Vec<Effect> {
        match self.modal_mut() {
            Some(Modal::Receipt(p)) => {
                p.id.clone_from(&sale.summary.id);
                p.sale = Some(Box::new(sale));
                Vec::new()
            }
            Some(Modal::Return(r)) => {
                r.text.clear();
                r.of_sale(sale);
                Vec::new()
            }
            Some(Modal::Docs(d)) => {
                let back = Some(Box::new(Docs {
                    number: String::new(),
                    ..d.clone()
                }));
                let id = sale.summary.id.clone();
                self.open(Modal::Receipt(Box::new(Preview {
                    sale: Some(Box::new(sale)),
                    id: id.clone(),
                    back,
                    ..Preview::default()
                })));
                self.quiet(Call::Receipt {
                    id,
                    copy: false,
                    override_token: None,
                })
            }
            _ => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sultan_client::{Problem, SaleLine};

    fn sold(total: i64, qty: i64) -> Sale {
        Sale {
            lines: vec![SaleLine {
                line_no: 1,
                variant_id: "v".into(),
                unit_id: "u".into(),
                name: "Teh".into(),
                qty_thousandths: qty,
                total,
                ..SaleLine::default()
            }],
            ..Sale::default()
        }
    }

    #[test]
    fn return_lines_refund_with_core() {
        let mut r = Return::default();
        r.of_sale(sold(10_000, 3_000));
        assert_eq!(r.step, ReturnStep::Lines);
        assert!(!r.bump(0, Some(true)));
        assert_eq!((r.lines[0].qty, r.lines[0].refund), (1_000, 3_333));
        r.bump(0, None);
        assert_eq!((r.lines[0].qty, r.refund()), (3_000, 10_000));
        r.bump(0, None);
        assert_eq!(r.lines[0].qty, 0);
        r.bump(0, Some(false));
        assert_eq!(r.lines[0].qty, 0);
        assert!(!r.bump(5, None));
        assert!(r.request().lines.is_empty());
        r.no_receipt = true;
        assert!(r.bump(0, Some(true)));
        assert_eq!(r.quote().lines[0].qty_thousandths, 1_000);
    }

    #[test]
    fn all_is_not_offered_without_a_receipt() {
        let mut r = Return {
            no_receipt: true,
            step: ReturnStep::Lines,
            lines: vec![ReturnLine {
                left: NO_RECEIPT_LEFT,
                qty: 2_000,
                ..ReturnLine::default()
            }],
            ..Return::default()
        };
        // `*` leaves the quantity alone and asks for no quote; `+` still counts up.
        assert!(!r.bump(0, None));
        assert_eq!(r.lines[0].qty, 2_000);
        assert!(r.bump(0, Some(true)));
        assert_eq!(r.lines[0].qty, 3_000);
        let mut c = at_return(r);
        let keys = |c: &Controller| -> Vec<&'static str> {
            c.after_shortcuts()
                .unwrap()
                .iter()
                .map(|k| k.label)
                .collect()
        };
        assert!(!keys(&c).contains(&"return_all"));
        assert!(c.after_action(Action::All).is_empty());
        let Some(Modal::Return(r)) = c.modal() else {
            panic!("return")
        };
        assert_eq!(r.lines[0].qty, 3_000);
    }

    /// A controller at home with the return dialog `r` open.
    fn at_return(r: Return) -> Controller {
        let (mut c, _) = Controller::new();
        c.login = Some(sultan_client::Login {
            token: "t".into(),
            user_id: "u".into(),
            display_name: "U".into(),
            branch_id: "b".into(),
            register_id: None,
            expires_at: 0,
        });
        let mut h = crate::controller::HomeScreen::default();
        h.sell.modal = Some(Modal::Return(Box::new(r)));
        c.screen = Screen::Home(Box::new(h));
        c
    }

    #[test]
    fn only_the_latest_return_quote_is_applied() {
        let r = Return {
            no_receipt: true,
            step: ReturnStep::Lines,
            lines: vec![ReturnLine {
                variant_id: "v".into(),
                unit_id: "u".into(),
                left: NO_RECEIPT_LEFT,
                qty: 1_000,
                ..ReturnLine::default()
            }],
            ..Return::default()
        };
        let mut c = at_return(r);
        let seq = |effects: &[Effect]| match effects {
            [
                Effect::After {
                    call: Call::Quote(seq, _),
                    ..
                },
            ] => *seq,
            other => panic!("{other:?}"),
        };
        let first = seq(&c.after_action(Action::More));
        let second = seq(&c.after_action(Action::More));
        assert_eq!((first, second), (1, 2));
        let quote = |total: i64| -> Quote {
            Quote {
                branch_id: "b".into(),
                date: "2027-01-15".into(),
                customer_id: None,
                tax_inclusive: true,
                tax_registered: true,
                cash_rounding: "none".into(),
                lines: vec![
                    serde_json::from_value(json!({
                        "variant_id": "v", "unit_id": "u", "qty_thousandths": 1000,
                        "mode": "fixed",
                        "tax": {"tax_class_id": "t", "rate_bp": 0, "tax": 0, "net": total,
                                "total": total}
                    }))
                    .unwrap(),
                ],
                totals: sultan_client::QuoteTotals::default(),
            }
        };
        let refund = |c: &Controller| match c.modal() {
            Some(Modal::Return(r)) => (r.refund(), r.quoting),
            _ => panic!("return"),
        };
        // The newer answer lands first; the older one, arriving last, is dropped.
        c.after_reply(c.generation, Reply::Quoted(second, Ok(quote(15_000))));
        assert_eq!(refund(&c), (15_000, false));
        c.after_reply(c.generation, Reply::Quoted(first, Ok(quote(10_000))));
        assert_eq!(refund(&c), (15_000, false));
        // A stale error does not end the running quote either.
        let third = seq(&c.after_action(Action::Less));
        c.after_reply(
            c.generation,
            Reply::Quoted(second, Err(Problem::new(500, "x", "x").into())),
        );
        assert!(refund(&c).1, "quote {third} still running");
        // With the dialog closed, an answer changes nothing.
        if let Screen::Home(h) = &mut c.screen {
            h.sell.modal = None;
        }
        assert!(
            c.after_reply(c.generation, Reply::Quoted(third, Ok(quote(5_000))))
                .is_empty()
        );
    }

    #[test]
    fn refund_customer_is_the_sales_own() {
        let mut r = Return {
            customer: Some("attached".into()),
            ..Return::default()
        };
        assert_eq!(r.refund_customer(), Some("attached"));
        let mut sale = sold(1, 1_000);
        sale.summary.customer_id = Some("owner".into());
        r.of_sale(sale);
        assert_eq!(r.refund_customer(), Some("owner"));
        assert_eq!(r.request().customer_id.as_deref(), Some("owner"));
    }

    #[test]
    fn errors_become_messages() {
        let e = |code: &str| -> ClientError { Problem::new(409, code, "x").into() };
        assert_eq!(after_notice(&e("cart_taken")).code, "cart_taken");
        assert_eq!(after_notice(&e("conflict")).code, "already_voided");
        assert_eq!(after_notice(&e("has_returns")).code, "has_returns");
        assert_eq!(after_notice(&e("not_found")).code, "receipt_not_found");
        assert_eq!(after_notice(&e("return_window")).code, "return_window");
        assert_eq!(
            after_notice(&e("return_qty_exceeded")).code,
            "return_qty_exceeded"
        );
        let field = |f: &str| -> ClientError {
            Problem::new(422, "invalid_field", "x")
                .with("field", f)
                .into()
        };
        assert_eq!(after_notice(&field("sale")).code, "other_branch_sale");
        assert_eq!(after_notice(&field("customer")).code, "needs_customer");
        assert_eq!(after_notice(&field("x")).code, "invalid_amount");
    }

    #[test]
    fn held_rows_read_the_payload() {
        let cart = HeldCart {
            id: "h".into(),
            register_id: "r1".into(),
            user_id: "u".into(),
            payload: json!({ "cart": { "customer": { "name": "Budi" } } }),
            item_count: 2,
            total: 5_000,
            note: Some("later".into()),
            ..HeldCart::default()
        };
        let row = held_row(&cart, Some("Ani"));
        assert_eq!(
            (
                row.register.as_str(),
                row.cashier.as_str(),
                row.customer.as_str()
            ),
            ("r1", "Ani", "Budi")
        );
        let row = held_row(&cart, None);
        assert_eq!(row.cashier, "u");
    }
}
