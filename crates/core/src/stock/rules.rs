//! Field rules of inventory data: stock reasons, adjustment lines, suppliers, the inventory
//! settings and which products can be on an inventory line (spec 2.8 §1, §3, §7, §13).

use super::{StockError, invalid};
use crate::catalog::{ProductType, Qty};

/// The longest document note, in characters (§1).
pub const MAX_NOTE_CHARS: usize = 500;
/// The longest stock reason name, in characters (§7).
pub const MAX_REASON_NAME_CHARS: usize = 60;
/// The longest supplier code, in characters (§3).
pub const MAX_SUPPLIER_CODE_CHARS: usize = 20;
/// The longest supplier name, in characters (§3).
pub const MAX_SUPPLIER_NAME_CHARS: usize = 120;
/// The default PPN rate of receipts, 11% (§13).
pub const DEFAULT_PPN_RATE_BP: u16 = 1_100;
/// The default expiry warning, in days (§13, I22).
pub const DEFAULT_EXPIRY_WARN_DAYS: u32 = 30;
/// The default velocity period, in days (§10).
pub const DEFAULT_VELOCITY_DAYS: u32 = 30;
/// The default cover days (§10).
pub const DEFAULT_COVER_DAYS: u32 = 14;
/// The largest number of days any setting or supplier field takes.
const MAX_DAYS: u32 = 365;

/// Only stock and weighed products are on inventory lines: a service has no stock and a bundle
/// is bought and counted as its components (§1).
///
/// # Errors
/// [`StockError::InvalidField`] (`variant`).
pub fn check_stock_type(product_type: ProductType) -> Result<(), StockError> {
    if product_type.has_stock() {
        Ok(())
    } else {
        Err(invalid(
            "variant",
            "services and bundles have no stock of their own",
        ))
    }
}

/// Trims a note and checks it is at most [`MAX_NOTE_CHARS`]; empty is `None`.
///
/// # Errors
/// [`StockError::InvalidField`] (`note`).
pub fn check_note(note: Option<&str>) -> Result<Option<String>, StockError> {
    let note = note.map(str::trim).unwrap_or_default();
    if note.chars().count() > MAX_NOTE_CHARS {
        return Err(invalid("note", "is at most 500 characters"));
    }
    Ok((!note.is_empty()).then(|| note.to_owned()))
}

/// Trims a stock reason name and checks it is 1–[`MAX_REASON_NAME_CHARS`] characters.
///
/// # Errors
/// [`StockError::InvalidField`] (`name`).
pub fn check_reason_name(name: &str) -> Result<String, StockError> {
    let name = name.trim();
    let n = name.chars().count();
    if n == 0 || n > MAX_REASON_NAME_CHARS {
        return Err(invalid("name", "must be 1–60 characters"));
    }
    Ok(name.to_owned())
}

/// Which way a stock reason moves stock (§7, I11).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ReasonDirection {
    /// Increases only.
    In,
    /// Decreases only.
    Out,
    /// Either way (correction).
    Both,
}

impl ReasonDirection {
    /// The stored name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::In => "in",
            Self::Out => "out",
            Self::Both => "both",
        }
    }

    /// The direction of a stored name.
    #[must_use]
    pub fn from_key(key: &str) -> Option<Self> {
        [Self::In, Self::Out, Self::Both]
            .into_iter()
            .find(|d| d.as_str() == key)
    }

    /// Whether a line of signed `qty` may use it.
    #[must_use]
    pub const fn allows(self, qty: Qty) -> bool {
        let t = qty.thousandths();
        match self {
            Self::In => t > 0,
            Self::Out => t < 0,
            Self::Both => t != 0,
        }
    }
}

/// A stock reason as the adjustment rules see it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StockReason {
    /// Its direction.
    pub direction: ReasonDirection,
    /// Offered for new lines (and not archived).
    pub active: bool,
    /// A line with it needs a note (the seeded Koreksi).
    pub note_required: bool,
}

/// A seeded stock reason (§7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SeedReason {
    /// The stored (Indonesian) name.
    pub name: &'static str,
    /// Its English meaning, for docs and tests.
    pub english: &'static str,
    /// Its direction.
    pub direction: ReasonDirection,
    /// Whether a line needs a note.
    pub note_required: bool,
    /// Its sort position.
    pub sort: i32,
}

/// The seeded stock reasons (§7): damaged, expired, lost, sample, own use (out) and correction
/// (both, with a note).
pub const SEED_REASONS: [SeedReason; 6] = [
    seed("Rusak", "damaged", ReasonDirection::Out, false, 10),
    seed("Kedaluwarsa", "expired", ReasonDirection::Out, false, 20),
    seed("Hilang", "lost", ReasonDirection::Out, false, 30),
    seed("Sampel", "sample", ReasonDirection::Out, false, 40),
    seed(
        "Pemakaian sendiri",
        "own use",
        ReasonDirection::Out,
        false,
        50,
    ),
    seed("Koreksi", "correction", ReasonDirection::Both, true, 90),
];

const fn seed(
    name: &'static str,
    english: &'static str,
    direction: ReasonDirection,
    note_required: bool,
    sort: i32,
) -> SeedReason {
    SeedReason {
        name,
        english,
        direction,
        note_required,
        sort,
    }
}

/// Checks one adjustment line (§7): a signed quantity ≠ 0, an active reason whose direction
/// allows the sign, and a note when the reason needs one. Returns the cleaned note.
///
/// # Errors
/// [`StockError::InvalidField`] (`qty`, `reason`, `note`).
pub fn check_adjust_line(
    qty: Qty,
    reason: StockReason,
    note: Option<&str>,
) -> Result<Option<String>, StockError> {
    if qty == Qty::ZERO {
        return Err(invalid("qty", "must not be 0"));
    }
    if !reason.active {
        return Err(invalid("reason", "is not active"));
    }
    if !reason.direction.allows(qty) {
        return Err(invalid("reason", "does not allow this direction"));
    }
    let note = check_note(note)?;
    if reason.note_required && note.is_none() {
        return Err(invalid("note", "is required for this reason"));
    }
    Ok(note)
}

/// The fields of a supplier the rules check (§3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SupplierInput<'a> {
    /// The code (1–20 characters, stored upper case).
    pub code: &'a str,
    /// The name (1–120 characters).
    pub name: &'a str,
    /// Payment terms in days (0 = cash).
    pub terms_days: u32,
    /// Lead time in days.
    pub lead_days: u32,
}

/// Checks a supplier (§3) and returns its cleaned `(code, name)`: the code trimmed and upper
/// case, without spaces; the name trimmed.
///
/// # Errors
/// [`StockError::InvalidField`] (`code`, `name`, `terms_days`, `lead_days`).
pub fn check_supplier(s: &SupplierInput<'_>) -> Result<(String, String), StockError> {
    let code = s.code.trim().to_uppercase();
    let n = code.chars().count();
    if n == 0 || n > MAX_SUPPLIER_CODE_CHARS || code.chars().any(char::is_whitespace) {
        return Err(invalid("code", "must be 1–20 characters without spaces"));
    }
    let name = s.name.trim();
    let n = name.chars().count();
    if n == 0 || n > MAX_SUPPLIER_NAME_CHARS {
        return Err(invalid("name", "must be 1–120 characters"));
    }
    if s.terms_days > MAX_DAYS {
        return Err(invalid("terms_days", "must be 0–365"));
    }
    if s.lead_days > MAX_DAYS {
        return Err(invalid("lead_days", "must be 0–365"));
    }
    Ok((code, name.to_owned()))
}

/// The company's inventory settings (§13).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct InventorySettings {
    /// The company is PKP: input PPN is credited and stays out of the cost (I5). Default false.
    pub pkp: bool,
    /// The receipts' default PPN rate in basis points (default 11%).
    pub standard_ppn_rate_bp: u16,
    /// Lots expiring within these days are listed (default 30).
    pub expiry_warn_days: u32,
    /// The velocity period N (default 30, > 0).
    pub velocity_days: u32,
    /// Cover days of a suggestion (default 14).
    pub cover_days: u32,
}

impl Default for InventorySettings {
    fn default() -> Self {
        Self {
            pkp: false,
            standard_ppn_rate_bp: DEFAULT_PPN_RATE_BP,
            expiry_warn_days: DEFAULT_EXPIRY_WARN_DAYS,
            velocity_days: DEFAULT_VELOCITY_DAYS,
            cover_days: DEFAULT_COVER_DAYS,
        }
    }
}

impl InventorySettings {
    /// Checks the ranges: rate ≤ 100%, warn and cover days 0–365, velocity days 1–365.
    ///
    /// # Errors
    /// [`StockError::InvalidField`] naming the field.
    pub fn check(&self) -> Result<(), StockError> {
        if self.standard_ppn_rate_bp > 10_000 {
            return Err(invalid("standard_ppn_rate", "must be at most 100%"));
        }
        if self.expiry_warn_days > MAX_DAYS {
            return Err(invalid("expiry_warn_days", "must be 0–365"));
        }
        if self.velocity_days == 0 || self.velocity_days > MAX_DAYS {
            return Err(invalid("velocity_days", "must be 1–365"));
        }
        if self.cover_days > MAX_DAYS {
            return Err(invalid("cover_days", "must be 0–365"));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_util::q;
    use super::*;

    #[test]
    fn stock_types() {
        assert_eq!(check_stock_type(ProductType::Stock), Ok(()));
        assert_eq!(check_stock_type(ProductType::Weighed), Ok(()));
        for t in [ProductType::Service, ProductType::Bundle] {
            assert_eq!(check_stock_type(t).unwrap_err().field(), Some("variant"));
        }
    }

    #[test]
    fn notes_and_names() {
        assert_eq!(check_note(None), Ok(None));
        assert_eq!(check_note(Some("  ")), Ok(None));
        assert_eq!(check_note(Some(" pecah ")), Ok(Some("pecah".into())));
        assert_eq!(
            check_note(Some(&"x".repeat(501))).unwrap_err().field(),
            Some("note")
        );
        assert_eq!(check_reason_name(" Rusak "), Ok("Rusak".into()));
        assert_eq!(check_reason_name(" ").unwrap_err().field(), Some("name"));
        assert_eq!(
            check_reason_name(&"x".repeat(61)).unwrap_err().field(),
            Some("name")
        );
    }

    #[test]
    fn seeds_and_directions() {
        let names: Vec<_> = SEED_REASONS.iter().map(|r| r.english).collect();
        assert_eq!(
            names,
            [
                "damaged",
                "expired",
                "lost",
                "sample",
                "own use",
                "correction"
            ]
        );
        assert!(
            SEED_REASONS[..5]
                .iter()
                .all(|r| r.direction == ReasonDirection::Out)
        );
        assert!(
            SEED_REASONS[5].note_required && SEED_REASONS[5].direction == ReasonDirection::Both
        );
        assert!(
            SEED_REASONS
                .iter()
                .all(|r| check_reason_name(r.name).is_ok())
        );
        for d in [
            ReasonDirection::In,
            ReasonDirection::Out,
            ReasonDirection::Both,
        ] {
            assert_eq!(ReasonDirection::from_key(d.as_str()), Some(d));
        }
        assert_eq!(ReasonDirection::from_key("drop"), None);
    }

    #[test]
    fn adjustment_lines() {
        type Case = (
            i64,
            StockReason,
            Option<&'static str>,
            Result<Option<&'static str>, &'static str>,
        );
        let out = StockReason {
            direction: ReasonDirection::Out,
            active: true,
            note_required: false,
        };
        let inn = StockReason {
            direction: ReasonDirection::In,
            ..out
        };
        let fix = StockReason {
            direction: ReasonDirection::Both,
            note_required: true,
            ..out
        };
        let off = StockReason {
            active: false,
            ..out
        };
        #[rustfmt::skip]
        let cases: Vec<Case> = vec![
            (-2, out, None, Ok(None)),
            (-2, out, Some(" pecah "), Ok(Some("pecah"))),
            (2, out, None, Err("reason")),
            (2, inn, None, Ok(None)),
            (-2, inn, None, Err("reason")),
            (0, fix, Some("x"), Err("qty")),
            (3, fix, Some("salah hitung"), Ok(Some("salah hitung"))),
            (-3, fix, Some("salah hitung"), Ok(Some("salah hitung"))),
            (-3, fix, None, Err("note")),
            (-3, fix, Some(" "), Err("note")),
            (-1, off, None, Err("reason")),
        ];
        for (n, reason, note, expected) in cases {
            let got = check_adjust_line(q(n), reason, note);
            let got = got
                .as_ref()
                .map(|n| n.as_deref())
                .map_err(|e| e.field().unwrap());
            assert_eq!(got, expected, "{n} {reason:?} {note:?}");
        }
    }

    #[test]
    fn suppliers() {
        let ok = SupplierInput {
            code: " sup-01 ",
            name: " PT Maju ",
            terms_days: 30,
            lead_days: 7,
        };
        assert_eq!(check_supplier(&ok), Ok(("SUP-01".into(), "PT Maju".into())));
        #[rustfmt::skip]
        let cases = [
            (SupplierInput { code: "", ..ok }, "code"),
            (SupplierInput { code: "A B", ..ok }, "code"),
            (SupplierInput { code: "ABCDEFGHIJKLMNOPQRSTU", ..ok }, "code"),
            (SupplierInput { name: " ", ..ok }, "name"),
            (SupplierInput { terms_days: 366, ..ok }, "terms_days"),
            (SupplierInput { lead_days: 366, ..ok }, "lead_days"),
        ];
        for (input, field) in cases {
            assert_eq!(
                check_supplier(&input).unwrap_err().field(),
                Some(field),
                "{input:?}"
            );
        }
        let long_name = "n".repeat(121);
        let s = SupplierInput {
            name: &long_name,
            ..ok
        };
        assert_eq!(check_supplier(&s).unwrap_err().field(), Some("name"));
    }

    #[test]
    fn settings() {
        let d = InventorySettings::default();
        assert_eq!(
            (
                d.pkp,
                d.standard_ppn_rate_bp,
                d.expiry_warn_days,
                d.velocity_days,
                d.cover_days
            ),
            (false, 1_100, 30, 30, 14)
        );
        assert_eq!(d.check(), Ok(()));
        #[rustfmt::skip]
        let cases = [
            (InventorySettings { standard_ppn_rate_bp: 10_001, ..d }, "standard_ppn_rate"),
            (InventorySettings { expiry_warn_days: 366, ..d }, "expiry_warn_days"),
            (InventorySettings { velocity_days: 0, ..d }, "velocity_days"),
            (InventorySettings { velocity_days: 366, ..d }, "velocity_days"),
            (InventorySettings { cover_days: 366, ..d }, "cover_days"),
        ];
        for (s, field) in cases {
            assert_eq!(s.check().unwrap_err().field(), Some(field));
        }
    }
}
