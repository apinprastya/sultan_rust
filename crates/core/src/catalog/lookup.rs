//! Register lookup: resolving a scanned or typed code to a variant + unit (spec 2.2 §6).
//!
//! The resolution order is pure; the data comes through the [`CatalogIndex`] trait, which the
//! repository (or a register's cache) implements.

use super::code::Plu;
use super::scale::{ScaleLayout, ScaleValue, decode_scale_barcode, qty_from_price};
use super::{CatalogError, Qty};
use crate::ids::{UnitId, VariantId};
use crate::money::Money;

/// A variant + unit: what every sale line references.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ItemRef {
    /// The variant.
    pub variant: VariantId,
    /// The unit.
    pub unit: UnitId,
}

/// Exact-match indexes of the catalog.
pub trait CatalogIndex {
    /// The variant + unit of a barcode, compared exactly as stored.
    fn by_barcode(&self, code: &str) -> Option<ItemRef>;
    /// The variant of an (upper-case) SKU, with its product's default sale unit.
    fn by_sku(&self, sku: &str) -> Option<ItemRef>;
    /// The weighed variant with this PLU, with its base unit.
    fn by_plu(&self, plu: Plu) -> Option<ItemRef>;
}

/// Which lookup step matched (spec 2.2 §6, steps 1–5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MatchSource {
    /// Step 1: the barcode as entered.
    Barcode,
    /// Step 2: the barcode after UPC-A ↔ EAN-13 leading-zero normalisation.
    NormalizedBarcode,
    /// Step 3: a scale barcode layout.
    ScaleBarcode,
    /// Step 4: the SKU.
    Sku,
    /// Step 5: the PLU typed on its own.
    Plu,
}

/// A successful lookup.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct LookupMatch {
    /// The variant + unit.
    pub item: ItemRef,
    /// Which step matched.
    pub source: MatchSource,
    /// The quantity: 1, or the weight of a weight scale code. `None` for a price-embedded scale
    /// code: it follows from the unit price, see [`LookupMatch::qty_at`].
    pub qty: Option<Qty>,
    /// The line amount embedded in a price scale code.
    pub amount: Option<Money>,
}

impl LookupMatch {
    /// The line quantity: [`LookupMatch::qty`] when known, else `amount / unit_price` rounded
    /// half-up to 3 decimal places (spec 2.2 §6).
    ///
    /// # Errors
    /// See [`qty_from_price`].
    pub fn qty_at(&self, unit_price: Money) -> Result<Qty, CatalogError> {
        match (self.qty, self.amount) {
            (Some(qty), _) => Ok(qty),
            (None, amount) => qty_from_price(amount.unwrap_or(Money::ZERO), unit_price),
        }
    }
}

fn found(item: ItemRef, source: MatchSource) -> LookupMatch {
    LookupMatch {
        item,
        source,
        qty: Some(Qty::ONE),
        amount: None,
    }
}

/// Resolves `input` (surrounding whitespace ignored), first match wins (spec 2.2 §6):
/// 1. exact barcode (also old codes that happen to start with a scale prefix);
/// 2. UPC-A ↔ EAN-13: 12 digits → `0` + code; 13 digits starting with `0` → without it;
/// 3. scale barcode layouts → PLU → variant with weight or amount;
/// 4. SKU (input upper-cased);
/// 5. PLU, only for 1–6 digit input.
///
/// # Errors
/// [`CatalogError::NotFound`]; [`CatalogError::ScaleValueZero`] for a scale code of a known
/// PLU with a zero value.
pub fn resolve_lookup(
    input: &str,
    layouts: &[ScaleLayout],
    index: &impl CatalogIndex,
) -> Result<LookupMatch, CatalogError> {
    let code = input.trim();
    if code.is_empty() {
        return Err(CatalogError::NotFound);
    }
    if let Some(item) = index.by_barcode(code) {
        return Ok(found(item, MatchSource::Barcode));
    }
    let all_digits = code.bytes().all(|b| b.is_ascii_digit());
    if let Some(item) = normalized_barcode(code).and_then(|n| index.by_barcode(&n)) {
        return Ok(found(item, MatchSource::NormalizedBarcode));
    }
    match decode_scale_barcode(code, layouts) {
        Ok(Some(scale)) => {
            if let Some(item) = index.by_plu(scale.plu) {
                let (qty, amount) = match scale.value {
                    ScaleValue::Weight(w) => (Some(w), None),
                    ScaleValue::Price(p) => (None, Some(p)),
                };
                return Ok(LookupMatch {
                    item,
                    source: MatchSource::ScaleBarcode,
                    qty,
                    amount,
                });
            }
        }
        Ok(None) => {}
        Err(zero) => {
            // Only an error when the PLU is known; otherwise the code may still be a SKU.
            if let Some(plu) = scale_plu(code, layouts)
                && index.by_plu(plu).is_some()
            {
                return Err(zero);
            }
        }
    }
    if let Some(item) = index.by_sku(&code.to_ascii_uppercase()) {
        return Ok(found(item, MatchSource::Sku));
    }
    if all_digits
        && code.len() <= 6
        && let Ok(plu) = Plu::parse(code)
        && let Some(item) = index.by_plu(plu)
    {
        return Ok(found(item, MatchSource::Plu));
    }
    Err(CatalogError::NotFound)
}

/// The UPC-A ↔ EAN-13 alternative of a scanned code (lookup step 2): 12 digits → `0` + code;
/// 13 digits starting with `0` → without it; anything else → `None`. Surrounding whitespace is
/// ignored.
#[must_use]
pub fn normalized_barcode(input: &str) -> Option<String> {
    let code = input.trim();
    if !code.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    match code.len() {
        12 => Some(format!("0{code}")),
        13 if code.starts_with('0') => Some(code[1..].to_owned()),
        _ => None,
    }
}

/// The code that matched, as the catalog stores it, for showing the cashier how a scan
/// resolved: the barcode as entered, its UPC-A ↔ EAN-13 alternative, the scale code as entered,
/// the upper-cased SKU, or the typed PLU (surrounding whitespace removed).
#[must_use]
pub fn matched_code(input: &str, source: MatchSource) -> String {
    let code = input.trim();
    match source {
        MatchSource::NormalizedBarcode => {
            normalized_barcode(code).unwrap_or_else(|| code.to_owned())
        }
        MatchSource::Sku => code.to_ascii_uppercase(),
        MatchSource::Barcode | MatchSource::ScaleBarcode | MatchSource::Plu => code.to_owned(),
    }
}

/// The PLU of a scale code whose value is zero (decoding it with a fake non-zero value).
fn scale_plu(code: &str, layouts: &[ScaleLayout]) -> Option<Plu> {
    let layout = layouts.iter().find(|l| code[..2] == l.prefix.to_string())?;
    let digits = &code[2..2 + usize::from(layout.plu_digits)];
    digits.parse::<u32>().ok().and_then(Plu::new)
}

/// What [`check_sellable`] needs to know about a found item at the requesting branch.
#[allow(clippy::struct_excessive_bools)] // independent facts about the item, not a state machine
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct ItemStatus {
    /// The product is archived.
    pub product_archived: bool,
    /// The variant is archived.
    pub variant_archived: bool,
    /// The product is blocked at this branch (spec 2.2 §10).
    pub blocked_at_branch: bool,
    /// The product's `sellable` flag.
    pub sellable: bool,
}

/// The post-lookup check, in this order: archived → `archived`; blocked at the branch →
/// `not_sold_here`; not sellable → `not_sellable` (spec 2.2 §6, §9, §10).
///
/// # Errors
/// [`CatalogError::Archived`], [`CatalogError::NotSoldHere`] or [`CatalogError::NotSellable`].
pub const fn check_sellable(status: ItemStatus) -> Result<(), CatalogError> {
    if status.product_archived || status.variant_archived {
        Err(CatalogError::Archived)
    } else if status.blocked_at_branch {
        Err(CatalogError::NotSoldHere)
    } else if !status.sellable {
        Err(CatalogError::NotSellable)
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::code::gtin_check_digit;
    use std::collections::HashMap;
    use uuid::Uuid;

    fn item(n: u128) -> ItemRef {
        ItemRef {
            variant: VariantId::from_uuid(Uuid::from_u128(n)),
            unit: UnitId::from_uuid(Uuid::from_u128(n)),
        }
    }

    fn ean13(body: &str) -> String {
        format!("{body}{}", gtin_check_digit(body).unwrap())
    }

    #[derive(Default)]
    struct Index {
        barcodes: HashMap<String, ItemRef>,
        skus: HashMap<String, ItemRef>,
        plus: HashMap<u32, ItemRef>,
    }

    impl CatalogIndex for Index {
        fn by_barcode(&self, code: &str) -> Option<ItemRef> {
            self.barcodes.get(code).copied()
        }
        fn by_sku(&self, sku: &str) -> Option<ItemRef> {
            self.skus.get(sku).copied()
        }
        fn by_plu(&self, plu: Plu) -> Option<ItemRef> {
            self.plus.get(&plu.value()).copied()
        }
    }

    /// Item 1: barcode `0036000291452` (EAN-13 form of a UPC-A); item 2: an old code that looks
    /// like a weight scale code; item 3: weighed, PLU 123; item 4: SKU `KAOS01-M-RED`;
    /// item 5: SKU `123456`; item 6: barcode `4006381333931`.
    fn index() -> Index {
        let mut idx = Index::default();
        idx.barcodes.insert("0036000291452".into(), item(1));
        idx.barcodes.insert(ean13("210012301250"), item(2));
        idx.barcodes.insert("4006381333931".into(), item(6));
        idx.barcodes.insert("12345678".into(), item(7));
        idx.plus.insert(123, item(3));
        idx.skus.insert("KAOS01-M-RED".into(), item(4));
        idx.skus.insert("123456".into(), item(5));
        idx
    }

    #[test]
    fn resolution_order() {
        let layouts = ScaleLayout::default_layouts();
        let idx = index();
        let one = Some(Qty::ONE);
        let cases = [
            // Exact barcode beats the scale parse.
            (
                ean13("210012301250"),
                Ok((item(2), MatchSource::Barcode, one, None)),
            ),
            (
                " 4006381333931 ".into(),
                Ok((item(6), MatchSource::Barcode, one, None)),
            ),
            // UPC-A scanned as 12 digits finds the stored EAN-13.
            (
                "036000291452".into(),
                Ok((item(1), MatchSource::NormalizedBarcode, one, None)),
            ),
            // EAN-13 with a leading 0 finds a stored 12-digit code.
            ("0012345678905".into(), Err(CatalogError::NotFound)),
            (
                ean13("210012302000"),
                Ok((
                    item(3),
                    MatchSource::ScaleBarcode,
                    Some(Qty::from_thousandths(2_000)),
                    None,
                )),
            ),
            (
                ean13("220012315000"),
                Ok((
                    item(3),
                    MatchSource::ScaleBarcode,
                    None,
                    Some(Money::from_minor(15_000)),
                )),
            ),
            // Unknown PLU in a scale code → continues and finds nothing.
            (ean13("210099901250"), Err(CatalogError::NotFound)),
            (
                "kaos01-m-red".into(),
                Ok((item(4), MatchSource::Sku, one, None)),
            ),
            // A SKU that is all digits beats the PLU.
            ("123456".into(), Ok((item(5), MatchSource::Sku, one, None))),
            ("123".into(), Ok((item(3), MatchSource::Plu, one, None))),
            ("000123".into(), Ok((item(3), MatchSource::Plu, one, None))),
            // PLU fallback only for 1–6 digits.
            ("0000123".into(), Err(CatalogError::NotFound)),
            ("12a".into(), Err(CatalogError::NotFound)),
            (String::new(), Err(CatalogError::NotFound)),
            ("   ".into(), Err(CatalogError::NotFound)),
            // Zero weight with a known PLU → scale_value_zero; unknown PLU → not found.
            (ean13("210012300000"), Err(CatalogError::ScaleValueZero)),
            (ean13("210099900000"), Err(CatalogError::NotFound)),
        ];
        for (code, expected) in cases {
            let got =
                resolve_lookup(&code, &layouts, &idx).map(|m| (m.item, m.source, m.qty, m.amount));
            assert_eq!(got, expected, "{code:?}");
        }
    }

    #[test]
    fn ean13_with_leading_zero_finds_stored_upc_a() {
        let mut idx = index();
        idx.barcodes.insert("036000291452".into(), item(8));
        idx.barcodes.remove("0036000291452");
        let m = resolve_lookup("0036000291452", &[], &idx).unwrap();
        assert_eq!(
            (m.item, m.source),
            (item(8), MatchSource::NormalizedBarcode)
        );
    }

    #[test]
    fn matched_codes() {
        #[rustfmt::skip]
        let cases = [
            (" 4006381333931 ", MatchSource::Barcode, "4006381333931"),
            ("036000291452", MatchSource::NormalizedBarcode, "0036000291452"),
            ("0036000291452", MatchSource::NormalizedBarcode, "036000291452"),
            ("12345", MatchSource::NormalizedBarcode, "12345"),
            ("2100123012503", MatchSource::ScaleBarcode, "2100123012503"),
            ("kaos01-m-red ", MatchSource::Sku, "KAOS01-M-RED"),
            ("123", MatchSource::Plu, "123"),
        ];
        for (input, source, expected) in cases {
            assert_eq!(matched_code(input, source), expected, "{input:?}");
        }
        assert_eq!(normalized_barcode("1234567890123"), None);
        assert_eq!(normalized_barcode("03600029145a"), None);
    }

    #[test]
    fn qty_at_price() {
        let layouts = ScaleLayout::default_layouts();
        let idx = index();
        let price = resolve_lookup(&ean13("220012315000"), &layouts, &idx).unwrap();
        assert_eq!(
            price.qty_at(Money::from_minor(120_000)),
            Ok(Qty::from_thousandths(125))
        );
        let weight = resolve_lookup(&ean13("210012301250"), &[], &idx).unwrap();
        assert_eq!(weight.qty_at(Money::from_minor(1)), Ok(Qty::ONE));
    }

    #[test]
    fn sellable_check_order() {
        let ok = ItemStatus {
            sellable: true,
            ..ItemStatus::default()
        };
        let cases = [
            (ok, Ok(())),
            (
                ItemStatus {
                    product_archived: true,
                    blocked_at_branch: true,
                    ..ok
                },
                Err(CatalogError::Archived),
            ),
            (
                ItemStatus {
                    variant_archived: true,
                    ..ok
                },
                Err(CatalogError::Archived),
            ),
            (
                ItemStatus {
                    blocked_at_branch: true,
                    sellable: false,
                    ..ok
                },
                Err(CatalogError::NotSoldHere),
            ),
            (
                ItemStatus {
                    sellable: false,
                    ..ok
                },
                Err(CatalogError::NotSellable),
            ),
        ];
        for (status, expected) in cases {
            assert_eq!(check_sellable(status), expected, "{status:?}");
        }
    }
}
