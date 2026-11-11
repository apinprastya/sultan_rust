//! The session home's barcode field (phase C5, spec 2.2 §6): what is typed, the lookup waiting
//! for its answer, and the item (or the reason) shown for the last code. Pure state; the
//! controller ([`crate::controller`]) decides when to look up, and the glue formats the shown
//! item with [`qty_text`] and [`match_kind`].
//!
//! - A scanner types the code and Enter, so the field takes digits and the other printable ASCII
//!   characters (letters for Code-128 and SKUs, `-`, `.`); space and control keys are not part of
//!   a code. At most [`MAX_CODE_LEN`] characters (the longest barcode).
//! - Enter hands the code to the lookup and empties the field at once, so the next scan can start
//!   while the answer is on its way. Only the answer to the newest lookup is shown; Esc forgets
//!   the field, the waiting lookup and the shown item.
//! - Prices (spec 2.3, phase R5): the lookup brings the store price of one unit
//!   (`Lookup::unit_price`, no customer). With a customer attached, the register also quotes the
//!   shown item for that customer ([`ScanField::start_quote`], `POST /pricing/quote`): qty 1 of
//!   the scanned unit, or the weight of a weight label. An open-price item and a price-embedded
//!   label are never quoted: their price is typed at sale or printed and final (spec 2.3 §5).
//!   A failed quote only marks the customer price; the store price stays shown.

use sultan_client::{Lookup, QuoteLine, QuotedLine};
use sultan_core::catalog::{MAX_BARCODE_LEN, Qty};
use sultan_core::money::Money;

use crate::controller::Notice;

/// Longest code the field takes (the longest barcode, spec 2.2 §6).
pub const MAX_CODE_LEN: usize = MAX_BARCODE_LEN;

/// What the last looked-up code resolved to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scanned {
    /// The code as it was sent.
    pub code: String,
    /// The item, or why there is none (`item_not_found`, `item_not_sold_here`, …).
    pub result: Result<Lookup, Notice>,
    /// The attached customer's price for the item.
    pub customer: CustomerPrice,
}

/// The attached customer's price for the shown item (spec 2.3 R5).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum CustomerPrice {
    /// No customer attached, or an item that is not quoted.
    #[default]
    None,
    /// The quote numbered with this sequence number is on its way.
    Waiting(u64),
    /// The quoted line.
    Priced(Box<QuotedLine>),
    /// The quote failed; the store price stays shown.
    Failed(Notice),
}

impl Scanned {
    /// The found item.
    #[must_use]
    pub fn item(&self) -> Option<&Lookup> {
        self.result.as_ref().ok()
    }
}

/// The barcode field; see the module docs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScanField {
    /// What is typed so far.
    pub code: String,
    /// The lookup waiting for its answer: its sequence number and code.
    pub waiting: Option<(u64, String)>,
    /// The answer to the last lookup.
    pub shown: Option<Scanned>,
}

impl ScanField {
    /// Types `c` if it can be part of a code and the field is not full. Returns whether it was.
    pub fn push(&mut self, c: char) -> bool {
        if !c.is_ascii_graphic() || self.code.len() >= MAX_CODE_LEN {
            return false;
        }
        self.code.push(c);
        true
    }

    /// Removes the last character.
    pub fn pop(&mut self) -> bool {
        self.code.pop().is_some()
    }

    /// Forgets everything: the typed code, the waiting lookup and the shown item.
    pub fn clear(&mut self) {
        *self = Self::default();
    }

    /// Takes the typed code for the lookup numbered `seq` (the field is emptied). `None` when
    /// nothing was typed.
    pub fn take(&mut self, seq: u64) -> Option<String> {
        let code = std::mem::take(&mut self.code);
        if code.is_empty() {
            return None;
        }
        self.waiting = Some((seq, code.clone()));
        Some(code)
    }

    /// Looks the shown code up again as lookup `seq` (after a catalog change), unless another
    /// lookup is waiting. Returns the code.
    pub fn again(&mut self, seq: u64) -> Option<String> {
        if self.waiting.is_some() {
            return None;
        }
        let code = self.shown.as_ref()?.code.clone();
        self.waiting = Some((seq, code.clone()));
        Some(code)
    }

    /// The answer of lookup `seq`: shown if it is the one waiting, else ignored (a newer scan or
    /// Esc came in between). Returns whether it was shown.
    pub fn answer(&mut self, seq: u64, result: Result<Lookup, Notice>) -> bool {
        match self.waiting.take() {
            Some((waiting, code)) if waiting == seq => {
                self.shown = Some(Scanned {
                    code,
                    result,
                    customer: CustomerPrice::None,
                });
                true
            }
            other => {
                self.waiting = other;
                false
            }
        }
    }

    /// Asks for the customer price of the shown item as quote `seq`: the line to quote, or `None`
    /// when no item is shown or it is not quoted ([`quote_line`]).
    pub fn start_quote(&mut self, seq: u64) -> Option<QuoteLine> {
        let shown = self.shown.as_mut()?;
        let line = quote_line(shown.result.as_ref().ok()?)?;
        shown.customer = CustomerPrice::Waiting(seq);
        Some(line)
    }

    /// The answer of quote `seq`: shown if it is the one waiting. Returns whether it was.
    pub fn quoted(&mut self, seq: u64, result: Result<QuotedLine, Notice>) -> bool {
        match &mut self.shown {
            Some(shown) if shown.customer == CustomerPrice::Waiting(seq) => {
                shown.customer = match result {
                    Ok(line) => CustomerPrice::Priced(Box::new(line)),
                    Err(notice) => CustomerPrice::Failed(notice),
                };
                true
            }
            _ => false,
        }
    }

    /// Forgets the customer price (the customer was detached).
    pub fn forget_quote(&mut self) {
        if let Some(shown) = &mut self.shown {
            shown.customer = CustomerPrice::None;
        }
    }

    /// The code of the waiting lookup, if any.
    #[must_use]
    pub fn looking(&self) -> Option<&str> {
        self.waiting.as_ref().map(|(_, code)| code.as_str())
    }
}

/// A quantity in thousandths as text: whole numbers plain (`2`), others with three decimals
/// and a decimal comma (`1,250`), as Indonesian receipts write them.
#[must_use]
pub fn qty_text(thousandths: i64) -> String {
    let sign = if thousandths < 0 { "-" } else { "" };
    let abs = thousandths.unsigned_abs();
    let (whole, fraction) = (abs / 1_000, abs % 1_000);
    if fraction == 0 {
        format!("{sign}{whole}")
    } else {
        format!("{sign}{whole},{fraction:03}")
    }
}

/// How the code matched, as a label code for the UI: `barcode`, `normalized_barcode`,
/// `scale_weight`, `scale_price`, `sku` or `plu` (the server's `source`, with scale codes split by
/// what they carry).
#[must_use]
pub fn match_kind(item: &Lookup) -> &str {
    match item.source.as_str() {
        "scale_barcode" if item.amount.is_some() => "scale_price",
        "scale_barcode" => "scale_weight",
        other => other,
    }
}

/// The line to quote for a found item: one of the scanned unit, or the weight of a weight
/// label. `None` for an open-price item and a price-embedded label (spec 2.3 §5: typed at sale,
/// or printed and final).
#[must_use]
pub fn quote_line(item: &Lookup) -> Option<QuoteLine> {
    if item.open_price || item.amount.is_some() {
        return None;
    }
    let qty = item.qty_thousandths.filter(|q| *q > 0).unwrap_or(1_000);
    Some(QuoteLine::new(&item.variant_id, &item.unit_id, qty))
}

/// `unit_price × qty`, rounded half up to Rp 1 (spec 2.3 §4.7); `None` on overflow.
#[must_use]
pub fn line_amount(unit_price: i64, qty_thousandths: i64) -> Option<i64> {
    sultan_core::pricing::line_amount(
        Money::from_minor(unit_price),
        Qty::from_thousandths(qty_thousandths),
    )
    .ok()
    .map(Money::minor)
}

/// How far `price` is from `store`, as a signed percentage with at most one decimal and a
/// decimal comma (`−5%`, `+2,5%`); empty without a store price or when they are equal.
#[must_use]
pub fn percent_off(price: i64, store: Option<i64>) -> String {
    let Some(store) = store.filter(|s| *s > 0) else {
        return String::new();
    };
    let diff = i128::from(price) - i128::from(store);
    if diff == 0 {
        return String::new();
    }
    // Tenths of a percent, rounded half away from zero.
    let scaled = diff.abs() * 1_000;
    let store = i128::from(store);
    let tenths = (scaled + store / 2) / store;
    let sign = if diff < 0 { '\u{2212}' } else { '+' };
    let (whole, fraction) = (tenths / 10, tenths % 10);
    if fraction == 0 {
        format!("{sign}{whole}%")
    } else {
        format!("{sign}{whole},{fraction}%")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::controller::Notice;

    fn item(source: &str, amount: Option<i64>) -> Lookup {
        Lookup {
            product_id: "p".into(),
            variant_id: "v".into(),
            unit_id: "u".into(),
            name: "Keju".into(),
            product_name: "Keju".into(),
            variant_label: String::new(),
            unit_name: "kg".into(),
            sku: "KEJU".into(),
            plu: Some(123),
            product_type: "weighed".into(),
            open_price: false,
            requires_note: false,
            source: source.into(),
            matched_code: "x".into(),
            qty_thousandths: None,
            amount,
            branch_id: None,
            unit_price: None,
        }
    }

    #[test]
    fn typing_takes_printable_ascii_up_to_the_limit() {
        let mut f = ScanField::default();
        for c in "Kp-1.x".chars() {
            assert!(f.push(c));
        }
        for c in [' ', '\n', 'é', '\u{F700}'] {
            assert!(!f.push(c), "{c:?}");
        }
        assert_eq!(f.code, "Kp-1.x");
        assert!(f.pop());
        assert_eq!(f.code, "Kp-1.");
        f.code = "9".repeat(MAX_CODE_LEN);
        assert!(!f.push('1'), "full");
        f.clear();
        assert!(!f.pop());
        assert_eq!(f, ScanField::default());
    }

    #[test]
    fn only_the_newest_answer_is_shown() {
        let mut f = ScanField::default();
        assert_eq!(f.take(1), None, "nothing typed");
        assert_eq!(f.again(1), None, "nothing shown");
        f.push('A');
        assert_eq!(f.take(1).as_deref(), Some("A"));
        assert_eq!((f.code.as_str(), f.looking()), ("", Some("A")));
        f.push('B');
        assert_eq!(f.take(2).as_deref(), Some("B"));
        // The answer to the first scan arrives late: ignored.
        assert!(!f.answer(1, Err(Notice::bad("item_not_found"))));
        assert_eq!(f.looking(), Some("B"));
        assert!(f.answer(2, Ok(item("barcode", None))));
        let shown = f.shown.as_ref().unwrap();
        assert_eq!((shown.code.as_str(), shown.item().is_some()), ("B", true));
        assert_eq!(f.looking(), None);
        // Nothing waiting: a stray answer changes nothing.
        assert!(!f.answer(3, Err(Notice::bad("item_not_found"))));
        // Again: the shown code, unless something is waiting.
        assert_eq!(f.again(4).as_deref(), Some("B"));
        assert_eq!(f.again(5), None);
        assert!(f.answer(4, Err(Notice::bad("item_archived"))));
        assert!(f.shown.as_ref().unwrap().item().is_none());
    }

    #[test]
    fn customer_prices_follow_the_shown_item() {
        let mut f = ScanField::default();
        assert_eq!(f.start_quote(1), None, "nothing shown");
        f.push('A');
        f.take(1);
        f.answer(1, Ok(item("plu", None)));
        let line = f.start_quote(1).unwrap();
        assert_eq!(
            (line.variant_id.as_str(), line.qty_thousandths),
            ("v", 1_000)
        );
        let shown = |f: &ScanField| f.shown.as_ref().unwrap().customer.clone();
        assert_eq!(shown(&f), CustomerPrice::Waiting(1));
        // A newer quote replaces it: the late answer is ignored.
        f.start_quote(2);
        assert!(!f.quoted(1, Err(Notice::bad("network"))));
        assert!(f.quoted(2, Err(Notice::bad("no_price"))));
        assert_eq!(shown(&f), CustomerPrice::Failed(Notice::bad("no_price")));
        assert!(
            !f.quoted(2, Err(Notice::bad("network"))),
            "answered already"
        );
        f.forget_quote();
        assert_eq!(shown(&f), CustomerPrice::None);
        // A failed lookup is never quoted.
        f.push('B');
        f.take(3);
        f.answer(3, Err(Notice::bad("item_not_found")));
        assert_eq!(f.start_quote(4), None);
        f.clear();
        f.forget_quote();
        assert!(!f.quoted(4, Err(Notice::bad("network"))));
    }

    #[test]
    fn what_is_quoted_and_how_far_from_the_store_price() {
        let mut weight = item("scale_barcode", None);
        weight.qty_thousandths = Some(1_250);
        assert_eq!(quote_line(&weight).unwrap().qty_thousandths, 1_250);
        assert_eq!(
            quote_line(&item("scale_barcode", Some(15_000))),
            None,
            "final amount"
        );
        let mut open = item("plu", None);
        open.open_price = true;
        assert_eq!(quote_line(&open), None, "typed at sale");
        #[rustfmt::skip]
        let cases = [
            (4_750, Some(5_000), "\u{2212}5%"), (9_928, Some(10_450), "\u{2212}5%"),
            (5_125, Some(5_000), "+2,5%"), (5_000, Some(5_000), ""), (1, None, ""),
            (1, Some(0), ""), (0, Some(3), "\u{2212}100%"),
        ];
        for (price, store, text) in cases {
            assert_eq!(percent_off(price, store), text, "{price} {store:?}");
        }
        assert_eq!(line_amount(12_990, 1_250), Some(16_238));
        assert_eq!(line_amount(i64::MAX, 2_000), None);
    }

    #[test]
    fn quantities_and_match_kinds() {
        #[rustfmt::skip]
        let cases = [
            (1_000, "1"), (24_000, "24"), (1_250, "1,250"), (5, "0,005"), (0, "0"),
            (-1_500, "-1,500"),
        ];
        for (thousandths, text) in cases {
            assert_eq!(qty_text(thousandths), text, "{thousandths}");
        }
        let cases = [
            (item("scale_barcode", Some(15_000)), "scale_price"),
            (item("scale_barcode", None), "scale_weight"),
            (item("normalized_barcode", None), "normalized_barcode"),
            (item("sku", None), "sku"),
        ];
        for (item, kind) in cases {
            assert_eq!(match_kind(&item), kind);
        }
    }
}
