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
//! - Prices are not part of the catalog (spec 2.3): the item panel only says what was found.

use sultan_client::Lookup;
use sultan_core::catalog::MAX_BARCODE_LEN;

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
                self.shown = Some(Scanned { code, result });
                true
            }
            other => {
                self.waiting = other;
                false
            }
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
