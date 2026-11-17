//! The receipt text (spec 2.6 §9): fixed-width lines for 32, 42 or 48 columns, so the screen
//! preview and the ESC/POS printer (2.11) print the same thing.
//!
//! The text that gets translated is not built in here: every label is a [`Label`] and the caller
//! passes a function that gives its text (the UI's ID/EN translation; [`Label::english`] is the
//! default). Amounts use Indonesian grouping without the currency (`1.250.000`), quantities a
//! decimal comma (`1,255`), like the register screens.
//!
//! Limitations: widths are counted in Unicode scalar values (`char`s), which is right for the
//! Latin text of receipts but not for wide (CJK) characters; a word longer than the width is cut.

use std::collections::BTreeMap;

use super::totals::CartTotals;
use crate::catalog::Qty;
use crate::customer::{RECEIPT_SEPARATOR, ReceiptCustomer};
use crate::doc_number::BranchTimeZone;
use crate::money::Money;
use crate::time::Timestamp;

/// The receipt width, a branch setting.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum ReceiptWidth {
    /// 58 mm paper.
    #[default]
    Cols32,
    /// 76 mm paper.
    Cols42,
    /// 80 mm paper.
    Cols48,
}

impl ReceiptWidth {
    /// The number of columns.
    #[must_use]
    pub const fn cols(self) -> usize {
        match self {
            Self::Cols32 => 32,
            Self::Cols42 => 42,
            Self::Cols48 => 48,
        }
    }

    /// The width with this many columns, if it is one of 32, 42, 48.
    #[must_use]
    pub const fn from_cols(cols: usize) -> Option<Self> {
        match cols {
            32 => Some(Self::Cols32),
            42 => Some(Self::Cols42),
            48 => Some(Self::Cols48),
            _ => None,
        }
    }
}

/// A translatable label of the receipt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Label {
    /// Reprint marker.
    Copy,
    /// Voided sale marker.
    Void,
    /// Before the branch's or customer's tax id.
    TaxId,
    /// Before the register code.
    Register,
    /// Before the cashier name.
    Cashier,
    /// A discount (line and totals).
    Discount,
    /// Σ gross.
    Subtotal,
    /// The tax base of a tax-exclusive sale.
    TaxBase,
    /// The tax of a tax-exclusive sale (followed by the rate).
    Tax,
    /// The tax contained in a tax-inclusive total.
    TaxIncluded,
    /// Cash rounding.
    Rounding,
    /// The total.
    Total,
    /// Change handed back.
    Change,
    /// Before a tender reference.
    Reference,
    /// Change saved to the customer balance.
    SavedToBalance,
    /// The customer's new balance.
    Balance,
    /// The exchange tender.
    Exchange,
}

impl Label {
    /// The English text.
    #[must_use]
    pub const fn english(self) -> &'static str {
        match self {
            Self::Copy => "COPY",
            Self::Void => "VOID",
            Self::TaxId => "NPWP",
            Self::Register => "Register",
            Self::Cashier => "Cashier",
            Self::Discount => "Discount",
            Self::Subtotal => "Subtotal",
            Self::TaxBase => "DPP",
            Self::Tax => "PPN",
            Self::TaxIncluded => "incl. PPN",
            Self::Rounding => "Rounding",
            Self::Total => "TOTAL",
            Self::Change => "Change",
            Self::Reference => "Ref",
            Self::SavedToBalance => "Saved to balance",
            Self::Balance => "Balance",
            Self::Exchange => "Exchange",
        }
    }

    /// The Indonesian text (spec 2.6 §19 from S4: a receipt prints in the window language).
    #[must_use]
    pub const fn indonesian(self) -> &'static str {
        match self {
            Self::Copy => "SALINAN",
            Self::Void => "BATAL",
            Self::TaxId => "NPWP",
            Self::Register => "Kasir",
            Self::Cashier => "Petugas",
            Self::Discount => "Diskon",
            Self::Subtotal => "Subtotal",
            Self::TaxBase => "DPP",
            Self::Tax => "PPN",
            Self::TaxIncluded => "termasuk PPN",
            Self::Rounding => "Pembulatan",
            Self::Total => "TOTAL",
            Self::Change => "Kembali",
            Self::Reference => "Ref",
            Self::SavedToBalance => "Disimpan ke saldo",
            Self::Balance => "Saldo",
            Self::Exchange => "Tukar barang",
        }
    }

    /// The text in `language` (`id` → Indonesian, anything else English).
    #[must_use]
    pub fn text(self, language: &str) -> &'static str {
        if language.starts_with("id") {
            self.indonesian()
        } else {
            self.english()
        }
    }
}

/// The branch block at the top.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReceiptHeader<'a> {
    /// The branch's receipt header lines (2.1).
    pub lines: &'a [String],
    /// The branch name.
    pub branch_name: &'a str,
    /// The branch address.
    pub address: &'a str,
    /// The company's NPWP, printed when tax-registered.
    pub tax_id: Option<&'a str>,
}

/// The full company block of a tax invoice (D13, 2.5 §10).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TaxInvoiceBlock<'a> {
    /// The company name.
    pub company_name: &'a str,
    /// The tax id (NPWP).
    pub tax_id: &'a str,
    /// The tax address.
    pub tax_address: &'a str,
}

/// Who the sale was for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReceiptCustomerBlock<'a> {
    /// The one-line customer form (2.5 §10).
    Customer(ReceiptCustomer<'a>),
    /// The full company block of a tax invoice sale.
    TaxInvoice(TaxInvoiceBlock<'a>),
}

/// One sold line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReceiptLine<'a> {
    /// The receipt name (product + variant).
    pub name: &'a str,
    /// The quantity; `None` for a price label without a derived quantity.
    pub qty: Option<Qty>,
    /// The unit's short name.
    pub unit: &'a str,
    /// The unit price.
    pub unit_price: Money,
    /// The gross amount (`unit_price × qty`, or the label amount).
    pub gross: Money,
    /// The line discount.
    pub discount: Money,
}

/// The tax of one rate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TaxLine {
    /// The rate in basis points.
    pub rate_bp: u16,
    /// The tax base (net).
    pub base: Money,
    /// The tax.
    pub tax: Money,
}

impl TaxLine {
    /// The tax per rate of a cart, rates above 0 only, lowest rate first.
    #[must_use]
    pub fn from_totals(totals: &CartTotals) -> Vec<Self> {
        let mut by_rate: BTreeMap<u16, (Money, Money)> = BTreeMap::new();
        for l in totals.lines.iter().filter(|l| l.tax_rate_bp > 0) {
            let e = by_rate.entry(l.tax_rate_bp).or_default();
            e.0 = e.0.saturating_add(l.net);
            e.1 = e.1.saturating_add(l.tax);
        }
        by_rate
            .into_iter()
            .map(|(rate_bp, (base, tax))| Self { rate_bp, base, tax })
            .collect()
    }
}

/// The totals block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReceiptTotals {
    /// Σ gross.
    pub subtotal: Money,
    /// Line + cart discounts.
    pub discount: Money,
    /// Tax per rate; empty when not tax-registered.
    pub tax: Vec<TaxLine>,
    /// Prices include tax.
    pub tax_inclusive: bool,
    /// The cash rounding.
    pub cash_rounding: Money,
    /// The total paid (after cash rounding).
    pub total: Money,
}

/// One tender.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReceiptTender<'a> {
    /// The tender type's name (for an exchange, use [`Label::Exchange`]'s text).
    pub name: &'a str,
    /// The amount it pays.
    pub amount: Money,
    /// Cash: what was handed over (printed instead of the amount).
    pub cash_received: Option<Money>,
    /// Cash: the change handed back.
    pub change_given: Money,
    /// The reference, printed masked.
    pub reference: Option<&'a str>,
}

/// Saved change and the new balance (2.5 §11).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ReceiptSaved {
    /// Saved to the balance.
    pub saved: Money,
    /// The balance after the sale.
    pub balance: Money,
}

/// Everything a receipt prints, in §9 order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReceiptInput<'a> {
    /// The width.
    pub width: ReceiptWidth,
    /// The branch block.
    pub header: ReceiptHeader<'a>,
    /// The sale number (also the barcode/QR payload).
    pub number: &'a str,
    /// When it was posted.
    pub posted_at: Timestamp,
    /// The branch's time zone.
    pub tz: BranchTimeZone,
    /// The register code.
    pub register: &'a str,
    /// The cashier's name.
    pub cashier: &'a str,
    /// The customer line or tax-invoice block.
    pub customer: Option<ReceiptCustomerBlock<'a>>,
    /// The lines.
    pub lines: &'a [ReceiptLine<'a>],
    /// The totals.
    pub totals: ReceiptTotals,
    /// The tenders.
    pub tenders: &'a [ReceiptTender<'a>],
    /// Saved change, if any.
    pub saved: Option<ReceiptSaved>,
    /// The branch footer lines.
    pub footer: &'a [String],
    /// A reprint.
    pub copy: bool,
    /// A voided sale.
    pub voided: bool,
}

/// The receipt: its text lines and the payload of the barcode/QR printed under them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Receipt {
    /// The text, each line at most the width.
    pub lines: Vec<String>,
    /// The sale number, for the barcode/QR that returns scan (§8).
    pub barcode: String,
}

impl Receipt {
    /// The text with a newline after each line.
    #[must_use]
    pub fn text(&self) -> String {
        let mut out = String::new();
        for line in &self.lines {
            out.push_str(line);
            out.push('\n');
        }
        out
    }
}

/// `1250000` → `1.250.000`, `-5000` → `-5.000` (Indonesian grouping, no currency).
#[must_use]
pub fn format_rupiah(minor: i64) -> String {
    let digits = minor.unsigned_abs().to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3 + 1);
    if minor < 0 {
        out.push('-');
    }
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push('.');
        }
        out.push(c);
    }
    out
}

/// `2.000` → `2`, `1.255` → `1,255`, `0.500` → `0,5` (decimal comma, trailing zeros dropped).
#[must_use]
pub fn format_qty(qty: Qty) -> String {
    let text = qty.to_string();
    let (whole, frac) = text.split_once('.').unwrap_or((&text, ""));
    let frac = frac.trim_end_matches('0');
    if frac.is_empty() {
        whole.to_owned()
    } else {
        format!("{whole},{frac}")
    }
}

/// A rate in basis points: `1100` → `11%`, `1150` → `11,5%`.
fn format_pct(bp: u16) -> String {
    let whole = bp / 100;
    let frac = format!("{:02}", bp % 100);
    let frac = frac.trim_end_matches('0');
    if frac.is_empty() {
        format!("{whole}%")
    } else {
        format!("{whole},{frac}%")
    }
}

/// Masks a reference to its last 4 characters: `123456789` → `*****6789`. Four characters or
/// fewer are printed as they are.
#[must_use]
pub fn mask_reference(reference: &str) -> String {
    let chars: Vec<char> = reference.trim().chars().collect();
    let keep = chars.len().saturating_sub(4);
    chars
        .iter()
        .enumerate()
        .map(|(i, c)| if i < keep { '*' } else { *c })
        .collect()
}

/// Line building with a fixed width.
struct Page {
    width: usize,
    lines: Vec<String>,
}

impl Page {
    fn rule(&mut self) {
        self.lines.push("-".repeat(self.width));
    }

    /// Word-wraps `text`; words longer than the width are cut. Leading spaces are an indent
    /// kept on every line (up to half the width).
    fn wrap(&self, text: &str) -> Vec<String> {
        let body = text.trim_start_matches(' ');
        let indent = (text.len() - body.len()).min(self.width / 2);
        let width = self.width - indent;
        let pad = " ".repeat(indent);
        let mut out = Vec::new();
        let mut cur = String::new();
        for word in body.split_whitespace() {
            let mut word: Vec<char> = word.chars().collect();
            while word.len() > width {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
                out.push(word.drain(..width).collect());
            }
            let word: String = word.into_iter().collect();
            let len = cur.chars().count();
            if len > 0 && len + 1 + word.chars().count() > width {
                out.push(std::mem::take(&mut cur));
            }
            if !cur.is_empty() {
                cur.push(' ');
            }
            cur.push_str(&word);
        }
        if !cur.is_empty() {
            out.push(cur);
        }
        out.into_iter().map(|l| format!("{pad}{l}")).collect()
    }

    fn left(&mut self, text: &str) {
        let lines = self.wrap(text);
        self.lines.extend(lines);
    }

    fn center(&mut self, text: &str) {
        for line in self.wrap(text) {
            let pad = (self.width - line.chars().count()) / 2;
            self.lines.push(format!("{}{line}", " ".repeat(pad)));
        }
    }

    /// `left` and `right` on one line, `right` right-aligned; when they do not fit together,
    /// `left` gets its own line(s) first.
    fn pair(&mut self, left: &str, right: &str) {
        let r = right.chars().count().min(self.width);
        let mut lines = self.wrap(left);
        let last = lines.pop().unwrap_or_default();
        self.lines.extend(lines);
        let l = last.chars().count();
        if l + 1 + r <= self.width {
            self.lines
                .push(format!("{last}{}{right}", " ".repeat(self.width - l - r)));
        } else {
            if !last.is_empty() {
                self.lines.push(last);
            }
            let right: String = right.chars().take(self.width).collect();
            self.lines
                .push(format!("{}{right}", " ".repeat(self.width - r)));
        }
    }

    fn money(&mut self, left: &str, amount: Money) {
        self.pair(left, &format_rupiah(amount.minor()));
    }
}

/// Builds the receipt text (§9), in this order: branch header, COPY / VOID marker, number,
/// local date and time, register and cashier, customer line or tax-invoice block, lines (name,
/// qty × unit price, gross, discount), subtotal, discount, tax, cash rounding, total, tenders
/// (references masked to the last 4), change, saved change with the new balance, footer. The
/// sale number is also returned as the barcode payload.
#[must_use]
pub fn build_receipt(input: &ReceiptInput<'_>, label: &dyn Fn(Label) -> String) -> Receipt {
    let mut p = Page {
        width: input.width.cols(),
        lines: Vec::new(),
    };
    head(&mut p, input, label);
    sold_lines(&mut p, input.lines, label);
    totals(&mut p, &input.totals, label);
    tenders(&mut p, input, label);
    if !input.footer.is_empty() {
        p.rule();
        for line in input.footer {
            p.center(line);
        }
    }
    if input.voided {
        p.center(&format!("*** {} ***", label(Label::Void)));
    }
    Receipt {
        lines: p.lines,
        barcode: input.number.to_owned(),
    }
}

/// Branch block, markers, number, date, register, cashier, customer.
fn head(p: &mut Page, input: &ReceiptInput<'_>, label: &dyn Fn(Label) -> String) {
    let marker = |l: Label| format!("*** {} ***", label(l));
    for line in input.header.lines {
        p.center(line);
    }
    p.center(input.header.branch_name);
    p.center(input.header.address);
    if let Some(tax_id) = input.header.tax_id {
        p.center(&format!("{} {tax_id}", label(Label::TaxId)));
    }
    if input.copy {
        p.center(&marker(Label::Copy));
    }
    if input.voided {
        p.center(&marker(Label::Void));
    }
    p.rule();
    p.left(input.number);
    p.left(
        &input
            .tz
            .local_datetime(input.posted_at)
            .format("%d/%m/%Y %H:%M")
            .to_string(),
    );
    p.pair(
        &format!("{} {}", label(Label::Register), input.register),
        &format!("{} {}", label(Label::Cashier), input.cashier),
    );
    match input.customer {
        Some(ReceiptCustomerBlock::Customer(c)) => p.left(&c.line()),
        Some(ReceiptCustomerBlock::TaxInvoice(t)) => {
            p.left(t.company_name);
            p.left(&format!("{} {}", label(Label::TaxId), t.tax_id));
            p.left(t.tax_address);
        }
        None => {}
    }
    p.rule();
}

/// Name, qty × unit price, gross and discount per line.
fn sold_lines(p: &mut Page, lines: &[ReceiptLine<'_>], label: &dyn Fn(Label) -> String) {
    for line in lines {
        p.left(line.name);
        let price = format_rupiah(line.unit_price.minor());
        let qty = match line.qty {
            Some(q) => format!("  {} {} x {price}", format_qty(q), line.unit),
            None => String::from("  "),
        };
        p.money(qty.trim_end(), line.gross);
        if line.discount > Money::ZERO {
            p.money(
                &format!("  {}", label(Label::Discount)),
                Money::from_minor(-line.discount.minor()),
            );
        }
    }
    p.rule();
}

/// Subtotal, discount, tax, cash rounding, total.
fn totals(p: &mut Page, t: &ReceiptTotals, label: &dyn Fn(Label) -> String) {
    p.money(&label(Label::Subtotal), t.subtotal);
    if t.discount > Money::ZERO {
        p.money(
            &label(Label::Discount),
            Money::from_minor(-t.discount.minor()),
        );
    }
    for tax in &t.tax {
        if t.tax_inclusive {
            p.money(
                &format!("{} {}", label(Label::TaxIncluded), format_pct(tax.rate_bp)),
                tax.tax,
            );
        } else {
            p.money(&label(Label::TaxBase), tax.base);
            p.money(
                &format!("{} {}", label(Label::Tax), format_pct(tax.rate_bp)),
                tax.tax,
            );
        }
    }
    if t.cash_rounding != Money::ZERO {
        p.money(&label(Label::Rounding), t.cash_rounding);
    }
    p.money(&label(Label::Total), t.total);
    p.rule();
}

/// Tenders with masked references, change, saved change and the new balance.
fn tenders(p: &mut Page, input: &ReceiptInput<'_>, label: &dyn Fn(Label) -> String) {
    let mut change = Money::ZERO;
    for tender in input.tenders {
        p.money(tender.name, tender.cash_received.unwrap_or(tender.amount));
        if let Some(reference) = tender.reference {
            p.left(&format!(
                "  {} {}",
                label(Label::Reference),
                mask_reference(reference)
            ));
        }
        change = change.saturating_add(tender.change_given);
    }
    if input.tenders.iter().any(|t| t.cash_received.is_some()) {
        p.money(&label(Label::Change), change);
    }
    if let Some(saved) = input.saved {
        let first = format!(
            "{}: Rp {}",
            label(Label::SavedToBalance),
            format_rupiah(saved.saved.minor())
        );
        let second = format!(
            "{}: Rp {}",
            label(Label::Balance),
            format_rupiah(saved.balance.minor())
        );
        let one = format!("{first}{RECEIPT_SEPARATOR}{second}");
        if one.chars().count() <= p.width {
            p.left(&one);
        } else {
            p.left(&first);
            p.left(&second);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::customer::Phone;
    use crate::sale::{TotalsLine, compute_totals};
    use crate::tax::TaxSettings;

    fn m(v: i64) -> Money {
        Money::from_minor(v)
    }

    fn english(l: Label) -> String {
        l.english().to_owned()
    }

    #[test]
    fn number_formats() {
        #[rustfmt::skip]
        let money = [(0, "0"), (999, "999"), (1_000, "1.000"), (1_250_000, "1.250.000"),
            (-5_000, "-5.000"), (i64::MIN, "-9.223.372.036.854.775.808")];
        for (v, s) in money {
            assert_eq!(format_rupiah(v), s);
        }
        let q = Qty::from_thousandths;
        for (t, s) in [
            (2_000, "2"),
            (1_255, "1,255"),
            (500, "0,5"),
            (1_250, "1,25"),
        ] {
            assert_eq!(format_qty(q(t)), s);
        }
        for (bp, s) in [
            (1_100, "11%"),
            (1_150, "11,5%"),
            (1_125, "11,25%"),
            (0, "0%"),
        ] {
            assert_eq!(format_pct(bp), s);
        }
        for (r, s) in [
            ("123456789", "*****6789"),
            ("1234", "1234"),
            (" 12 ", "12"),
            ("", ""),
        ] {
            assert_eq!(mask_reference(r), s);
        }
        for w in [
            ReceiptWidth::Cols32,
            ReceiptWidth::Cols42,
            ReceiptWidth::Cols48,
        ] {
            assert_eq!(ReceiptWidth::from_cols(w.cols()), Some(w));
        }
        assert_eq!(ReceiptWidth::from_cols(40), None);
        assert_eq!(ReceiptWidth::default().cols(), 32);
    }

    fn sample<'a>(
        lines: &'a [ReceiptLine<'a>],
        tenders: &'a [ReceiptTender<'a>],
        header: &'a [String],
        footer: &'a [String],
    ) -> ReceiptInput<'a> {
        ReceiptInput {
            width: ReceiptWidth::Cols32,
            header: ReceiptHeader {
                lines: header,
                branch_name: "Toko Sultan Kemang",
                address: "Jl. Kemang Raya No. 1, Jakarta Selatan",
                tax_id: Some("01.234.567.8-901.000"),
            },
            number: "JKT01-R2-261116-0001",
            // 2026-11-16T07:05:00Z = 14:05 WIB.
            posted_at: Timestamp::from_unix_millis(1_794_787_200_000 + 7 * 3_600_000 + 300_000),
            tz: BranchTimeZone::default(),
            register: "R2",
            cashier: "Budi",
            customer: None,
            lines,
            totals: ReceiptTotals {
                subtotal: m(50_000),
                discount: m(3_550),
                tax: vec![TaxLine {
                    rate_bp: 1_100,
                    base: m(41_847),
                    tax: m(4_603),
                }],
                tax_inclusive: true,
                cash_rounding: m(50),
                total: m(46_500),
            },
            tenders,
            saved: None,
            footer,
            copy: false,
            voided: false,
        }
    }

    #[test]
    fn full_receipt_32_columns() {
        let lines = [
            ReceiptLine {
                name: "Aqua 600ml",
                qty: Some(Qty::from_thousandths(2_000)),
                unit: "pcs",
                unit_price: m(5_000),
                gross: m(10_000),
                discount: m(0),
            },
            ReceiptLine {
                name: "Beras Pandan Wangi Premium Super 5 kg",
                qty: Some(Qty::from_thousandths(1_000)),
                unit: "sak",
                unit_price: m(40_000),
                gross: m(40_000),
                discount: m(2_000),
            },
        ];
        let tenders = [
            ReceiptTender {
                name: "QRIS BRI",
                amount: m(20_000),
                cash_received: None,
                change_given: m(0),
                reference: Some("998877665544"),
            },
            ReceiptTender {
                name: "Tunai",
                amount: m(26_500),
                cash_received: Some(m(30_000)),
                change_given: m(2_800),
                reference: None,
            },
        ];
        let header = ["SULTAN MART".to_owned()];
        let footer = ["Terima kasih".to_owned()];
        let phone: Phone = "0812-3456-789".parse().unwrap();
        let mut input = sample(&lines, &tenders, &header, &footer);
        input.customer = Some(ReceiptCustomerBlock::Customer(ReceiptCustomer {
            name: "Budi Santoso",
            company_name: None,
            phone: Some(&phone),
            tier_name: Some("Gold"),
        }));
        input.saved = Some(ReceiptSaved {
            saved: m(700),
            balance: m(2_450),
        });
        input.copy = true;
        let r = build_receipt(&input, &english);
        let expected = [
            "          SULTAN MART",
            "       Toko Sultan Kemang",
            " Jl. Kemang Raya No. 1, Jakarta",
            "            Selatan",
            "   NPWP 01.234.567.8-901.000",
            "          *** COPY ***",
            "--------------------------------",
            "JKT01-R2-261116-0001",
            "16/11/2026 14:05",
            "Register R2         Cashier Budi",
            "Budi S. · 0812****789 · Gold",
            "--------------------------------",
            "Aqua 600ml",
            "  2 pcs x 5.000           10.000",
            "Beras Pandan Wangi Premium Super",
            "5 kg",
            "  1 sak x 40.000          40.000",
            "  Discount                -2.000",
            "--------------------------------",
            "Subtotal                  50.000",
            "Discount                  -3.550",
            "incl. PPN 11%              4.603",
            "Rounding                      50",
            "TOTAL                     46.500",
            "--------------------------------",
            "QRIS BRI                  20.000",
            "  Ref ********5544",
            "Tunai                     30.000",
            "Change                     2.800",
            "Saved to balance: Rp 700",
            "Balance: Rp 2.450",
            "--------------------------------",
            "          Terima kasih",
        ];
        assert_eq!(r.lines, expected);
        assert!(r.lines.iter().all(|l| l.chars().count() <= 32));
        assert_eq!(r.barcode, "JKT01-R2-261116-0001");
        assert!(r.text().ends_with("Terima kasih\n"));

        // A pair that does not fit is split, the right part right-aligned.
        input.cashier = "Bambang Sutrisno";
        let r = build_receipt(&input, &english);
        let at = r.lines.iter().position(|l| l == "Register R2").unwrap();
        assert_eq!(r.lines[at + 1], "        Cashier Bambang Sutrisno");
    }

    #[test]
    fn tax_invoice_exclusive_void_48_columns() {
        let lines = [ReceiptLine {
            name: "Timbang: Apel Fuji",
            qty: None,
            unit: "kg",
            unit_price: m(0),
            gross: m(15_000),
            discount: m(0),
        }];
        let tenders = [ReceiptTender {
            name: "Debit BCA",
            amount: m(16_650),
            cash_received: None,
            change_given: m(0),
            reference: Some("1234"),
        }];
        let mut input = sample(&lines, &tenders, &[], &[]);
        input.width = ReceiptWidth::Cols48;
        input.header.tax_id = None;
        input.voided = true;
        input.customer = Some(ReceiptCustomerBlock::TaxInvoice(TaxInvoiceBlock {
            company_name: "PT Maju Jaya",
            tax_id: "0123456789012345",
            tax_address: "Jl. Sudirman 5, Jakarta",
        }));
        input.totals = ReceiptTotals {
            subtotal: m(15_000),
            discount: m(0),
            tax: vec![TaxLine {
                rate_bp: 1_100,
                base: m(15_000),
                tax: m(1_650),
            }],
            tax_inclusive: false,
            cash_rounding: m(0),
            total: m(16_650),
        };
        input.saved = Some(ReceiptSaved {
            saved: m(700),
            balance: m(2_450),
        });
        let r = build_receipt(&input, &|l| match l {
            Label::Void => "BATAL".to_owned(),
            other => other.english().to_owned(),
        });
        let text = r.text();
        assert!(r.lines.iter().all(|l| l.chars().count() <= 48));
        assert_eq!(
            r.lines
                .iter()
                .filter(|l| l.contains("*** BATAL ***"))
                .count(),
            2
        );
        for want in [
            "PT Maju Jaya",
            "NPWP 0123456789012345",
            "Jl. Sudirman 5, Jakarta",
            &format!("DPP{}15.000", " ".repeat(39)),
            &format!("PPN 11%{}1.650", " ".repeat(36)),
            "  Ref 1234",
            "Saved to balance: Rp 700 · Balance: Rp 2.450",
        ] {
            assert!(text.contains(want), "missing {want:?} in\n{text}");
        }
        // No change line without cash, no NPWP of the branch, no rounding line.
        assert!(!text.contains("Change"));
        assert!(!text.contains("Rounding"));
        assert!(!text.contains("Discount"));
        assert!(!text.contains("NPWP 01."));
        // A label line without qty prints its amount only.
        assert!(r.lines.iter().any(|l| l.trim() == "15.000"));
    }

    #[test]
    fn wrapping_edges() {
        let p = Page {
            width: 8,
            lines: Vec::new(),
        };
        assert_eq!(p.wrap("abcdefghijkl mn"), ["abcdefgh", "ijkl mn"]);
        assert_eq!(p.wrap("ab abcdefghij"), ["ab", "abcdefgh", "ij"]);
        assert!(p.wrap("   ").is_empty());
        let mut p = p;
        // A right part wider than the page is cut.
        p.pair("", "1234567890");
        p.pair("abc", "12345");
        assert_eq!(p.lines, ["12345678", "abc", "   12345"]);
    }

    #[test]
    fn tax_lines_from_totals() {
        let line = |gross, rate| TotalsLine {
            gross: m(gross),
            line_discount: None,
            cart_discountable: true,
            tax_rate_bp: rate,
        };
        let totals = compute_totals(
            &[
                line(11_100, 1_100),
                line(5_000, 0),
                line(22_200, 1_100),
                line(1_200, 1_200),
            ],
            None,
            TaxSettings::default(),
        )
        .unwrap();
        assert_eq!(
            TaxLine::from_totals(&totals),
            [
                TaxLine {
                    rate_bp: 1_100,
                    base: m(30_000),
                    tax: m(3_300)
                },
                TaxLine {
                    rate_bp: 1_200,
                    base: m(1_071),
                    tax: m(129)
                },
            ]
        );
    }

    #[test]
    fn every_label_has_english_text() {
        use Label::*;
        for l in [
            Copy,
            Void,
            TaxId,
            Register,
            Cashier,
            Discount,
            Subtotal,
            TaxBase,
            Tax,
            TaxIncluded,
            Rounding,
            Total,
            Change,
            Reference,
            SavedToBalance,
            Balance,
            Exchange,
        ] {
            assert!(!l.english().is_empty());
            assert!(!l.indonesian().is_empty());
            assert_eq!(l.text("en"), l.english());
            assert_eq!(l.text("id"), l.indonesian());
        }
        assert_eq!(Label::Change.text("id-ID"), "Kembali");
    }
}
