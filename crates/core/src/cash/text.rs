//! The printable texts of spec 2.7 (K7, §5, §6, §11): the cash movement and payout slip, the X
//! report, the close report and the Z report.
//!
//! Built like the receipts ([`crate::sale::build_receipt`]): fixed-width lines for 32, 42 or 48
//! columns, every label a [`CashLabel`] whose text the caller gives (the window language;
//! [`CashLabel::text`] has Indonesian and English), amounts with Indonesian grouping. The screen
//! shows the same text until 2.11 prints it; lines in [`Printout::large`] are printed double size.

use super::figures::{CloseFigures, SessionFigures, XReport};
use super::movement::CashKind;
use super::z::{ZReport, ZSessionLine, ZTotals};
use crate::doc_number::{BranchTimeZone, LocalDay};
use crate::money::Money;
use crate::sale::{
    CountAmount, Page, ReceiptHeader, ReceiptWidth, TenderKey, format_pct, format_rupiah,
};
use crate::time::Timestamp;

/// A translatable label of the cash texts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CashLabel {
    /// Reprint marker.
    Copy,
    /// Before the register code.
    Register,
    /// Before the cashier name.
    Cashier,
    /// Before the approving manager.
    ApprovedBy,
    /// Slip title of a cash in.
    CashIn,
    /// Slip title of a cash out.
    CashOut,
    /// Slip title of a safe drop.
    SafeDrop,
    /// Slip title of a balance payout.
    Payout,
    /// Slip title of a reversal.
    Reversal,
    /// Before the reversed movement's number.
    ReversalOf,
    /// Before the reason.
    Reason,
    /// Before the note.
    Note,
    /// Before the customer.
    Customer,
    /// The customer's balance after a payout.
    Balance,
    /// Signature line of the one who receives the cash.
    ReceivedBy,
    /// Signature line of the one who gives the cash.
    GivenBy,
    /// The X report title, which says it is not a close.
    XReport,
    /// The close report title.
    CloseReport,
    /// The Z report title.
    ZReport,
    /// A supplementary Z (K16).
    Supplementary,
    /// A force-closed session.
    Forced,
    /// Before the open time.
    Opened,
    /// Before the close time.
    Closed,
    /// Before the X's read time.
    ReadAt,
    /// Before the Z's business day.
    BusinessDay,
    /// Before the Z's run time.
    RunAt,
    /// Before who ran the Z.
    RunBy,
    /// Opening cash.
    OpeningCash,
    /// Cash received on sales.
    CashReceived,
    /// Change handed back.
    ChangeGiven,
    /// Cash refunds.
    CashRefunds,
    /// Cash in (report line).
    CashInTotal,
    /// Cash out (report line).
    CashOutTotal,
    /// Safe drops (report line).
    Drops,
    /// Balance payouts (report line).
    Payouts,
    /// Expected cash.
    ExpectedCash,
    /// Counted cash.
    CountedCash,
    /// Over / short.
    OverShort,
    /// Float kept.
    FloatKept,
    /// To the safe.
    ToSafe,
    /// The proposed opening cash.
    OpeningProposed,
    /// Float difference.
    FloatDifference,
    /// The tenders block.
    Tenders,
    /// Sales of a tender.
    Sales,
    /// Refunds of a tender.
    Refunds,
    /// Cash rounding.
    Rounding,
    /// Line voids.
    LineVoids,
    /// Cart voids.
    CartVoids,
    /// Post-voids.
    PostVoids,
    /// Returns.
    Returns,
    /// Line discounts.
    LineDiscounts,
    /// Cart discounts.
    CartDiscounts,
    /// Price overrides.
    PriceOverrides,
    /// Change saved to balances.
    SavedChange,
    /// Paid with customer balances.
    BalancePaid,
    /// Reversals.
    Reversals,
    /// No-sale drawer opens.
    DrawerOpens,
    /// X reads.
    XReads,
    /// Cash outs without a nota.
    MissingNota,
    /// The Z's session list.
    Sessions,
    /// The Z's branch totals.
    BranchTotal,
    /// Number of sales.
    SalesCount,
    /// The first sale number.
    FirstSale,
    /// The last sale number.
    LastSale,
    /// Gross sales.
    GrossSales,
    /// Net sales.
    NetSales,
    /// The tax base (DPP).
    TaxBase,
    /// The tax (PPN), followed by the rate.
    Tax,
    /// Cash to deposit.
    CashToDeposit,
}

impl CashLabel {
    /// Every label.
    pub const ALL: [Self; 69] = [
        Self::Copy,
        Self::Register,
        Self::Cashier,
        Self::ApprovedBy,
        Self::CashIn,
        Self::CashOut,
        Self::SafeDrop,
        Self::Payout,
        Self::Reversal,
        Self::ReversalOf,
        Self::Reason,
        Self::Note,
        Self::Customer,
        Self::Balance,
        Self::ReceivedBy,
        Self::GivenBy,
        Self::XReport,
        Self::CloseReport,
        Self::ZReport,
        Self::Supplementary,
        Self::Forced,
        Self::Opened,
        Self::Closed,
        Self::ReadAt,
        Self::BusinessDay,
        Self::RunAt,
        Self::RunBy,
        Self::OpeningCash,
        Self::CashReceived,
        Self::ChangeGiven,
        Self::CashRefunds,
        Self::CashInTotal,
        Self::CashOutTotal,
        Self::Drops,
        Self::Payouts,
        Self::ExpectedCash,
        Self::CountedCash,
        Self::OverShort,
        Self::FloatKept,
        Self::ToSafe,
        Self::OpeningProposed,
        Self::FloatDifference,
        Self::Tenders,
        Self::Sales,
        Self::Refunds,
        Self::Rounding,
        Self::LineVoids,
        Self::CartVoids,
        Self::PostVoids,
        Self::Returns,
        Self::LineDiscounts,
        Self::CartDiscounts,
        Self::PriceOverrides,
        Self::SavedChange,
        Self::BalancePaid,
        Self::Reversals,
        Self::DrawerOpens,
        Self::XReads,
        Self::MissingNota,
        Self::Sessions,
        Self::BranchTotal,
        Self::SalesCount,
        Self::FirstSale,
        Self::LastSale,
        Self::GrossSales,
        Self::NetSales,
        Self::TaxBase,
        Self::Tax,
        Self::CashToDeposit,
    ];

    /// The English text and the Indonesian text.
    const fn texts(self) -> (&'static str, &'static str) {
        match self {
            Self::Copy => ("COPY", "SALINAN"),
            Self::Register => ("Register", "Kasir"),
            Self::Cashier => ("Cashier", "Petugas"),
            Self::ApprovedBy => ("Approved by", "Disetujui oleh"),
            Self::CashIn => ("CASH IN", "KAS MASUK"),
            Self::CashOut => ("CASH OUT", "KAS KELUAR"),
            Self::SafeDrop => ("SAFE DROP", "SETOR KE BRANKAS"),
            Self::Payout => ("BALANCE PAYOUT", "PENCAIRAN SALDO"),
            Self::Reversal => ("REVERSAL", "PEMBATALAN"),
            Self::ReversalOf => ("Reversal of", "Pembatalan dari"),
            Self::Reason => ("Reason", "Alasan"),
            Self::Note => ("Note", "Catatan"),
            Self::Customer => ("Customer", "Pelanggan"),
            Self::Balance => ("Balance", "Saldo"),
            Self::ReceivedBy => ("Received by", "Diterima oleh"),
            Self::GivenBy => ("Given by", "Diserahkan oleh"),
            Self::XReport => ("X REPORT — not a close", "LAPORAN X — bukan tutup kas"),
            Self::CloseReport => ("CLOSE REPORT", "LAPORAN TUTUP KAS"),
            Self::ZReport => ("Z REPORT", "LAPORAN Z"),
            Self::Supplementary => ("SUPPLEMENTARY", "SUSULAN"),
            Self::Forced => ("FORCED CLOSE", "TUTUP PAKSA"),
            Self::Opened => ("Opened", "Dibuka"),
            Self::Closed => ("Closed", "Ditutup"),
            Self::ReadAt => ("Read at", "Dibaca"),
            Self::BusinessDay => ("Business day", "Hari usaha"),
            Self::RunAt => ("Run at", "Dijalankan"),
            Self::RunBy => ("Run by", "Oleh"),
            Self::OpeningCash => ("Opening cash", "Modal awal"),
            Self::CashReceived => ("Cash received", "Tunai diterima"),
            Self::ChangeGiven => ("Change given", "Kembalian"),
            Self::CashRefunds => ("Cash refunds", "Pengembalian tunai"),
            Self::CashInTotal => ("Cash in", "Kas masuk"),
            Self::CashOutTotal => ("Cash out", "Kas keluar"),
            Self::Drops => ("Safe drops", "Setor ke brankas"),
            Self::Payouts => ("Balance payouts", "Pencairan saldo"),
            Self::ExpectedCash => ("EXPECTED CASH", "KAS SEHARUSNYA"),
            Self::CountedCash => ("Counted cash", "Kas dihitung"),
            Self::OverShort => ("Over / short", "Selisih"),
            Self::FloatKept => ("Kept as float", "Modal disimpan"),
            Self::ToSafe => ("To the safe", "Ke brankas"),
            Self::OpeningProposed => ("Proposed opening", "Modal diusulkan"),
            Self::FloatDifference => ("Float difference", "Selisih modal"),
            Self::Tenders => ("Tenders", "Pembayaran"),
            Self::Sales => ("Sales", "Penjualan"),
            Self::Refunds => ("Refunds", "Pengembalian"),
            Self::Rounding => ("Rounding", "Pembulatan"),
            Self::LineVoids => ("Line voids", "Batal baris"),
            Self::CartVoids => ("Cart voids", "Batal keranjang"),
            Self::PostVoids => ("Post-voids", "Batal nota"),
            Self::Returns => ("Returns", "Retur"),
            Self::LineDiscounts => ("Line discounts", "Diskon baris"),
            Self::CartDiscounts => ("Cart discounts", "Diskon keranjang"),
            Self::PriceOverrides => ("Price overrides", "Ubah harga"),
            Self::SavedChange => ("Saved change", "Kembalian disimpan"),
            Self::BalancePaid => ("Paid with balance", "Dibayar dengan saldo"),
            Self::Reversals => ("Reversals", "Pembatalan"),
            Self::DrawerOpens => ("Drawer opens", "Laci dibuka"),
            Self::XReads => ("X reads", "Laporan X dibaca"),
            Self::MissingNota => ("Missing nota", "Tanpa nota"),
            Self::Sessions => ("SESSIONS", "SESI"),
            Self::BranchTotal => ("BRANCH TOTAL", "TOTAL CABANG"),
            Self::SalesCount => ("Sales count", "Jumlah penjualan"),
            Self::FirstSale => ("First sale", "Nota pertama"),
            Self::LastSale => ("Last sale", "Nota terakhir"),
            Self::GrossSales => ("Gross sales", "Penjualan kotor"),
            Self::NetSales => ("Net sales", "Penjualan bersih"),
            Self::TaxBase => ("DPP", "DPP"),
            Self::Tax => ("PPN", "PPN"),
            Self::CashToDeposit => ("CASH TO DEPOSIT", "KAS UNTUK DISETOR"),
        }
    }

    /// The English text.
    #[must_use]
    pub const fn english(self) -> &'static str {
        self.texts().0
    }

    /// The Indonesian text.
    #[must_use]
    pub const fn indonesian(self) -> &'static str {
        self.texts().1
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

/// A printable text: its lines and which of them print double size.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Printout {
    /// The text, each line at most the width.
    pub lines: Vec<String>,
    /// Indexes into `lines` that print in large text (the slip's amount, the Z's cash to
    /// deposit).
    pub large: Vec<usize>,
}

impl Printout {
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

/// What every cash text needs besides its content.
#[derive(Clone, Copy)]
pub struct TextOptions<'a> {
    /// The width.
    pub width: ReceiptWidth,
    /// The branch's time zone, for the local times.
    pub tz: BranchTimeZone,
    /// The label texts (e.g. `|l| l.text("id").to_owned()`).
    pub label: &'a dyn Fn(CashLabel) -> String,
    /// The name of a tender in the tender block (the tender type's name).
    pub tender_name: &'a dyn Fn(TenderKey) -> String,
}

/// The title of a slip.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SlipKind {
    /// CASH IN.
    CashIn,
    /// CASH OUT.
    CashOut,
    /// SAFE DROP.
    Drop,
    /// BALANCE PAYOUT (2.5 §11), in the same layout.
    Payout,
}

impl From<CashKind> for SlipKind {
    fn from(kind: CashKind) -> Self {
        match kind {
            CashKind::In => Self::CashIn,
            CashKind::Out => Self::CashOut,
            CashKind::Drop => Self::Drop,
        }
    }
}

impl SlipKind {
    const fn label(self) -> CashLabel {
        match self {
            Self::CashIn => CashLabel::CashIn,
            Self::CashOut => CashLabel::CashOut,
            Self::Drop => CashLabel::SafeDrop,
            Self::Payout => CashLabel::Payout,
        }
    }
}

/// Everything a slip prints (§2, K7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SlipInput<'a> {
    /// The branch block.
    pub header: ReceiptHeader<'a>,
    /// Cash in, out, drop or payout.
    pub kind: SlipKind,
    /// The movement number (a payout: its reference).
    pub number: &'a str,
    /// When it was posted.
    pub posted_at: Timestamp,
    /// The register code.
    pub register: &'a str,
    /// The cashier's name.
    pub cashier: &'a str,
    /// The approving manager (override).
    pub approved_by: Option<&'a str>,
    /// For a reversal: the reversed movement's number.
    pub reversal_of: Option<&'a str>,
    /// The reason's name.
    pub reason: Option<&'a str>,
    /// The note.
    pub note: Option<&'a str>,
    /// A payout: the customer line.
    pub customer: Option<&'a str>,
    /// A payout: the balance after it.
    pub balance_after: Option<Money>,
    /// The amount.
    pub amount: Money,
    /// A reprint.
    pub copy: bool,
}

/// Who and when, at the top of the X and close reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionHeader<'a> {
    /// The session number.
    pub number: &'a str,
    /// The register code.
    pub register: &'a str,
    /// The cashier's name.
    pub cashier: &'a str,
    /// Opened at.
    pub opened_at: Timestamp,
    /// Closed at (`None` on an X).
    pub closed_at: Option<Timestamp>,
    /// Force-closed.
    pub forced: bool,
    /// The approving manager (an X read by override, a force close).
    pub approved_by: Option<&'a str>,
}

/// Everything an X report prints (§5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct XReportInput<'a> {
    /// The branch block.
    pub header: ReceiptHeader<'a>,
    /// The session.
    pub session: SessionHeader<'a>,
    /// The figures.
    pub report: &'a XReport,
}

/// Everything a close report prints (§4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CloseReportInput<'a> {
    /// The branch block.
    pub header: ReceiptHeader<'a>,
    /// The session.
    pub session: SessionHeader<'a>,
    /// The stored figures.
    pub report: &'a CloseFigures,
    /// A reprint.
    pub copy: bool,
}

/// Everything a Z report prints (§6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ZReportInput<'a> {
    /// The branch block.
    pub header: ReceiptHeader<'a>,
    /// The Z.
    pub report: &'a ZReport,
    /// Who ran it.
    pub run_by: &'a str,
    /// The approving manager (override at the till).
    pub approved_by: Option<&'a str>,
    /// A reprint.
    pub copy: bool,
}

struct Text<'a> {
    page: Page,
    large: Vec<usize>,
    opts: &'a TextOptions<'a>,
}

impl<'a> Text<'a> {
    fn new(opts: &'a TextOptions<'a>) -> Self {
        Self {
            page: Page {
                width: opts.width.cols(),
                lines: Vec::new(),
            },
            large: Vec::new(),
            opts,
        }
    }

    fn l(&self, label: CashLabel) -> String {
        (self.opts.label)(label)
    }

    fn time(&self, at: Timestamp) -> String {
        self.opts
            .tz
            .local_datetime(at)
            .format("%d/%m/%Y %H:%M")
            .to_string()
    }

    fn header(&mut self, header: &ReceiptHeader<'_>, copy: bool) {
        for line in header.lines {
            self.page.center(line);
        }
        self.page.center(header.branch_name);
        self.page.center(header.address);
        if copy {
            self.marker(CashLabel::Copy);
        }
    }

    fn marker(&mut self, label: CashLabel) {
        let text = format!("*** {} ***", self.l(label));
        self.page.center(&text);
    }

    fn field(&mut self, label: CashLabel, value: &str) {
        let text = format!("{}: {value}", self.l(label));
        self.page.left(&text);
    }

    fn money(&mut self, label: CashLabel, amount: Money) {
        let text = self.l(label);
        self.page.money(&text, amount);
    }

    /// A money line with a minus sign for what left the drawer.
    fn minus(&mut self, label: &str, amount: Money) {
        self.page.money(
            label,
            Money::from_minor(0_i64.saturating_sub(amount.minor())),
        );
    }

    fn count(&mut self, label: CashLabel, ca: CountAmount, negative: bool) {
        let text = format!("{} ({})", self.l(label), ca.count);
        if negative {
            self.minus(&text, ca.amount);
        } else {
            self.page.money(&text, ca.amount);
        }
    }

    fn number(&mut self, label: CashLabel, n: u32) {
        let text = self.l(label);
        self.page.pair(&text, &n.to_string());
    }

    fn large(&mut self, text: &str) {
        self.page.center(text);
        self.large.push(self.page.lines.len() - 1);
    }

    fn pair_labels(&mut self, left: CashLabel, left_value: &str, right: CashLabel, value: &str) {
        let l = format!("{} {left_value}", self.l(left));
        let r = format!("{} {value}", self.l(right));
        self.page.pair(&l, &r);
    }

    fn finish(self) -> Printout {
        Printout {
            lines: self.page.lines,
            large: self.large,
        }
    }

    fn session(&mut self, s: &SessionHeader<'_>) {
        self.page.left(s.number);
        self.pair_labels(
            CashLabel::Register,
            s.register,
            CashLabel::Cashier,
            s.cashier,
        );
        let opened = self.time(s.opened_at);
        self.field(CashLabel::Opened, &opened);
        if let Some(at) = s.closed_at {
            let closed = self.time(at);
            self.field(CashLabel::Closed, &closed);
        }
        if let Some(by) = s.approved_by {
            self.field(CashLabel::ApprovedBy, by);
        }
    }

    /// The figures shared by the X, the close report and the Z (§4).
    fn figures(&mut self, f: &SessionFigures) {
        let s = &f.sales;
        self.money(CashLabel::OpeningCash, s.opening_cash);
        self.money(CashLabel::CashReceived, s.cash_received);
        let change = self.l(CashLabel::ChangeGiven);
        self.minus(&change, s.change_given);
        let refunds = self.l(CashLabel::CashRefunds);
        self.minus(&refunds, s.cash_refunds);
        self.count(CashLabel::CashInTotal, f.cash.cash_in, false);
        let lines = f.cash.reason_lines();
        self.reasons(&lines, super::Direction::In);
        self.count(CashLabel::CashOutTotal, f.cash.cash_out, true);
        self.reasons(&lines, super::Direction::Out);
        self.count(CashLabel::Drops, f.cash.drops, true);
        let payouts = self.l(CashLabel::Payouts);
        self.minus(&payouts, s.payouts);
        self.money(CashLabel::ExpectedCash, s.expected_cash);
        self.page.rule();
        if !s.tenders.is_empty() {
            let title = self.l(CashLabel::Tenders);
            self.page.left(&title);
            for (key, t) in &s.tenders {
                let name = (self.opts.tender_name)(*key);
                self.page.left(&format!("  {name}"));
                for (label, ca, negative) in [
                    (CashLabel::Sales, t.sales, false),
                    (CashLabel::Refunds, t.refunds, true),
                ] {
                    if ca.count > 0 {
                        let text = format!("    {} ({})", self.l(label), ca.count);
                        if negative {
                            self.minus(&text, ca.amount);
                        } else {
                            self.page.money(&text, ca.amount);
                        }
                    }
                }
            }
        }
        if s.cash_rounding != Money::ZERO {
            self.money(CashLabel::Rounding, s.cash_rounding);
        }
        for (label, ca) in [
            (CashLabel::LineVoids, s.line_voids),
            (CashLabel::CartVoids, s.cart_voids),
            (CashLabel::PostVoids, s.post_voids),
            (CashLabel::Returns, s.returns),
            (CashLabel::LineDiscounts, s.line_discounts),
            (CashLabel::CartDiscounts, s.cart_discounts),
            (CashLabel::PriceOverrides, s.price_overrides),
            (CashLabel::Reversals, f.cash.reversals),
        ] {
            if ca.count > 0 {
                self.count(label, ca, false);
            }
        }
        for (label, amount) in [
            (CashLabel::SavedChange, s.saved_change),
            (CashLabel::BalancePaid, s.balance_paid),
        ] {
            if amount != Money::ZERO {
                self.money(label, amount);
            }
        }
        self.number(CashLabel::DrawerOpens, f.drawer_opens);
        self.number(CashLabel::XReads, f.x_reports);
        self.number(CashLabel::MissingNota, f.missing_nota);
    }

    fn reasons(&mut self, lines: &[&super::ReasonLine], direction: super::Direction) {
        for r in lines.iter().filter(|r| r.direction == direction) {
            let text = if r.reversals > 0 {
                format!(
                    "  {} ({}, {} {})",
                    r.name,
                    r.count,
                    self.l(CashLabel::Reversals),
                    r.reversals
                )
            } else {
                format!("  {} ({})", r.name, r.count)
            };
            if direction == super::Direction::Out {
                self.minus(&text, r.amount);
            } else {
                self.page.money(&text, r.amount);
            }
        }
    }

    fn float(&mut self, opening_proposed: Option<Money>, difference: Money) {
        if let Some(p) = opening_proposed {
            self.money(CashLabel::OpeningProposed, p);
        }
        if opening_proposed.is_some() || difference != Money::ZERO {
            self.money(CashLabel::FloatDifference, difference);
        }
    }
}

/// Builds a cash movement or payout slip (§2, K7): branch header, COPY marker, the title (CASH
/// IN / CASH OUT / SAFE DROP / BALANCE PAYOUT; a reversal prints REVERSAL above it and the
/// original's number), number, local time, register and cashier, approver, customer, reason,
/// note, the amount in large text, the balance after a payout, and the "Received by" and "Given
/// by" signature lines.
#[must_use]
pub fn build_slip(input: &SlipInput<'_>, opts: &TextOptions<'_>) -> Printout {
    let mut t = Text::new(opts);
    t.header(&input.header, input.copy);
    if input.reversal_of.is_some() {
        t.marker(CashLabel::Reversal);
        let title = t.l(input.kind.label());
        t.page.center(&title);
    } else {
        t.marker(input.kind.label());
    }
    t.page.rule();
    t.page.left(input.number);
    let at = t.time(input.posted_at);
    t.page.left(&at);
    t.pair_labels(
        CashLabel::Register,
        input.register,
        CashLabel::Cashier,
        input.cashier,
    );
    for (label, value) in [
        (CashLabel::ApprovedBy, input.approved_by),
        (CashLabel::ReversalOf, input.reversal_of),
        (CashLabel::Customer, input.customer),
        (CashLabel::Reason, input.reason),
        (CashLabel::Note, input.note),
    ] {
        if let Some(value) = value {
            t.field(label, value);
        }
    }
    t.page.rule();
    t.large(&format!("Rp {}", format_rupiah(input.amount.minor())));
    if let Some(balance) = input.balance_after {
        t.money(CashLabel::Balance, balance);
    }
    t.page.rule();
    let received = t.l(CashLabel::ReceivedBy);
    let given = t.l(CashLabel::GivenBy);
    t.page.pair(&received, &given);
    t.page.lines.extend([String::new(), String::new()]);
    let half = t.page.width / 2 - 1;
    let line = "_".repeat(half);
    t.page.pair(&line, &line);
    t.finish()
}

/// Builds the X report text (§5): branch header, "X REPORT — not a close", the session, the
/// read time and approver, the figures with the expected cash, and the float difference. There
/// is no count and no over/short.
#[must_use]
pub fn build_x_report(input: &XReportInput<'_>, opts: &TextOptions<'_>) -> Printout {
    let mut t = Text::new(opts);
    t.header(&input.header, false);
    t.marker(CashLabel::XReport);
    t.page.rule();
    t.session(&input.session);
    let read = t.time(input.report.read_at);
    t.field(CashLabel::ReadAt, &read);
    t.page.rule();
    t.figures(&input.report.session);
    t.float(input.report.opening_proposed, input.report.float_difference);
    t.finish()
}

/// Builds the close report text (§4, the "later" of spec 2.1 §5): branch header, COPY, the
/// title, FORCED CLOSE, the session, the figures, counted cash and over/short, then the float
/// kept, to the safe, the proposed opening and the float difference.
#[must_use]
pub fn build_close_report(input: &CloseReportInput<'_>, opts: &TextOptions<'_>) -> Printout {
    let mut t = Text::new(opts);
    t.header(&input.header, input.copy);
    t.marker(CashLabel::CloseReport);
    if input.session.forced {
        t.marker(CashLabel::Forced);
    }
    t.page.rule();
    t.session(&input.session);
    t.page.rule();
    let r = input.report;
    t.figures(&r.session);
    t.page.rule();
    t.money(CashLabel::CountedCash, r.counted_cash);
    t.money(CashLabel::OverShort, r.over_short);
    t.money(CashLabel::FloatKept, r.float.float_kept);
    t.money(CashLabel::ToSafe, r.float.to_safe);
    t.float(r.float.opening_proposed, r.float.float_difference);
    t.finish()
}

fn day_text(day: LocalDay) -> String {
    let (y, m, d) = day.ymd();
    format!("{d:02}/{m:02}/{y}")
}

/// Builds the Z report text (§6): branch header, COPY, the title and SUPPLEMENTARY, number,
/// business day, run time, who ran and approved it, the sessions, the subtotals of each register
/// and the branch totals, ending with the cash to deposit in large text.
#[must_use]
pub fn build_z_report(input: &ZReportInput<'_>, opts: &TextOptions<'_>) -> Printout {
    let mut t = Text::new(opts);
    let z = input.report;
    t.header(&input.header, input.copy);
    t.marker(CashLabel::ZReport);
    if z.supplementary {
        t.marker(CashLabel::Supplementary);
    }
    t.page.rule();
    t.page.left(&z.number);
    t.field(CashLabel::BusinessDay, &day_text(z.day));
    let run = t.time(z.run_at);
    t.field(CashLabel::RunAt, &run);
    t.field(CashLabel::RunBy, input.run_by);
    if let Some(by) = input.approved_by {
        t.field(CashLabel::ApprovedBy, by);
    }
    t.page.rule();
    let title = format!("{} ({})", t.l(CashLabel::Sessions), z.sessions.len());
    t.page.left(&title);
    for s in &z.sessions {
        z_session(&mut t, s);
    }
    for (register, totals) in &z.registers {
        t.page.rule();
        let title = format!("== {} {register} ==", t.l(CashLabel::Register));
        t.page.center(&title);
        z_totals(&mut t, totals);
    }
    t.page.rule();
    let title = format!("== {} ==", t.l(CashLabel::BranchTotal));
    t.page.center(&title);
    z_totals(&mut t, &z.branch);
    let deposit = t.l(CashLabel::CashToDeposit);
    t.page.center(&deposit);
    t.large(&format!(
        "Rp {}",
        format_rupiah(z.branch.cash_to_deposit.minor())
    ));
    t.finish()
}

fn z_session(t: &mut Text<'_>, s: &ZSessionLine) {
    t.page.left(&s.number);
    let who = format!(
        "  {} {} · {}",
        t.l(CashLabel::Register),
        s.register,
        s.cashier
    );
    t.page.left(&who);
    let short = |at| {
        t.opts
            .tz
            .local_datetime(at)
            .format("%d/%m %H:%M")
            .to_string()
    };
    let when = format!("  {} - {}", short(s.opened_at), short(s.closed_at));
    t.page.left(&when);
    if s.forced {
        let forced = format!("  ({})", t.l(CashLabel::Forced));
        t.page.left(&forced);
    }
    for (label, amount) in [
        (CashLabel::OpeningCash, s.opening_cash),
        (CashLabel::ExpectedCash, s.expected_cash),
        (CashLabel::CountedCash, s.counted_cash),
        (CashLabel::OverShort, s.over_short),
        (CashLabel::FloatKept, s.float_kept),
        (CashLabel::ToSafe, s.to_safe),
        (CashLabel::FloatDifference, s.float_difference),
    ] {
        if label != CashLabel::FloatDifference || amount != Money::ZERO {
            let text = format!("  {}", t.l(label));
            t.page.money(&text, amount);
        }
    }
}

fn z_totals(t: &mut Text<'_>, z: &ZTotals) {
    t.number(CashLabel::Sessions, z.sessions);
    t.number(CashLabel::SalesCount, z.sales_count);
    if let Some(first) = &z.first_sale {
        t.field(CashLabel::FirstSale, &first.number);
    }
    if let Some(last) = &z.last_sale {
        t.field(CashLabel::LastSale, &last.number);
    }
    t.money(CashLabel::GrossSales, z.gross_sales);
    t.money(CashLabel::NetSales, z.net_sales);
    for tax in &z.tax {
        let pct = format_pct(tax.rate_bp);
        let base = format!("{} {pct}", t.l(CashLabel::TaxBase));
        t.page.money(&base, tax.base);
        let amount = format!("{} {pct}", t.l(CashLabel::Tax));
        t.page.money(&amount, tax.tax);
    }
    t.page.rule();
    t.figures(&z.figures);
    t.page.rule();
    t.money(CashLabel::CountedCash, z.counted_cash);
    t.money(CashLabel::OverShort, z.over_short);
    t.money(CashLabel::FloatKept, z.float_kept);
    t.money(CashLabel::ToSafe, z.to_safe);
    t.float(None, z.float_difference);
    t.money(CashLabel::CashToDeposit, z.cash_to_deposit);
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::cash::figures::tests::{case1, m};
    use crate::cash::z::tests::{NOV19_UTC, scripted, sessions};
    use crate::cash::{
        SessionInput, build_z, business_day, close_figures, session_figures, x_report,
    };
    use crate::sale::TenderKind;

    const HOUR: i64 = 3_600_000;

    fn english(l: CashLabel) -> String {
        l.english().to_owned()
    }

    fn indonesian(l: CashLabel) -> String {
        l.indonesian().to_owned()
    }

    fn tender_name(k: TenderKey) -> String {
        match k.kind {
            TenderKind::Cash => "Tunai".to_owned(),
            other => other.as_str().to_owned(),
        }
    }

    fn opts<'a>(
        width: ReceiptWidth,
        label: &'a dyn Fn(CashLabel) -> String,
        tender: &'a dyn Fn(TenderKey) -> String,
    ) -> TextOptions<'a> {
        TextOptions {
            width,
            tz: BranchTimeZone::default(),
            label,
            tender_name: tender,
        }
    }

    const HEADER: ReceiptHeader<'static> = ReceiptHeader {
        lines: &[],
        branch_name: "Toko Sultan Kemang",
        address: "Jl. Kemang Raya No. 1",
        tax_id: None,
    };

    fn at(hours: i64) -> Timestamp {
        Timestamp::from_unix_millis(NOV19_UTC + hours * HOUR)
    }

    fn assert_fits(p: &Printout, width: usize) {
        for line in &p.lines {
            assert!(line.chars().count() <= width, "{line:?} wider than {width}");
        }
    }

    #[test]
    fn slip_32_columns() {
        let input = SlipInput {
            header: HEADER,
            kind: CashKind::Out.into(),
            number: "JKT01-R2-K-261119-01",
            // 07:05Z = 14:05 WIB.
            posted_at: Timestamp::from_unix_millis(NOV19_UTC + 7 * HOUR + 300_000),
            register: "R2",
            cashier: "Budi",
            approved_by: None,
            reversal_of: None,
            reason: Some("Belanja keperluan toko"),
            note: Some("galon"),
            customer: None,
            balance_after: None,
            amount: m(25_000),
            copy: false,
        };
        let o = opts(ReceiptWidth::Cols32, &english, &tender_name);
        let p = build_slip(&input, &o);
        let expected = [
            "       Toko Sultan Kemang",
            "     Jl. Kemang Raya No. 1",
            "        *** CASH OUT ***",
            "--------------------------------",
            "JKT01-R2-K-261119-01",
            "19/11/2026 14:05",
            "Register R2         Cashier Budi",
            "Reason: Belanja keperluan toko",
            "Note: galon",
            "--------------------------------",
            "           Rp 25.000",
            "--------------------------------",
            "Received by             Given by",
            "",
            "",
            "_______________  _______________",
        ];
        assert_eq!(p.lines, expected);
        assert_eq!(p.large, [10]);
        assert!(p.text().ends_with("_______________\n"));
    }

    /// Name, input, width, labels → texts present, texts absent.
    type SlipCase<'a> = (
        &'a str,
        SlipInput<'a>,
        ReceiptWidth,
        &'a dyn Fn(CashLabel) -> String,
        &'a [&'a str],
        &'a [&'a str],
    );

    #[test]
    fn slip_variants() {
        let base = SlipInput {
            header: HEADER,
            kind: SlipKind::Drop,
            number: "JKT01-R2-K-261119-02",
            posted_at: at(3),
            register: "R2",
            cashier: "Budi",
            approved_by: Some("Manajer Sari"),
            reversal_of: None,
            reason: None,
            note: None,
            customer: None,
            balance_after: None,
            amount: m(300_000),
            copy: true,
        };
        #[rustfmt::skip]
        let cases: [SlipCase<'_>; 5] = [
            ("drop, copy, approver", base, ReceiptWidth::Cols32, &english,
                &["*** COPY ***", "*** SAFE DROP ***", "Approved by: Manajer Sari", "Rp 300.000"],
                &["REVERSAL", "Reason"]),
            ("indonesian drop", base, ReceiptWidth::Cols48, &indonesian,
                &["*** SALINAN ***", "*** SETOR KE BRANKAS ***", "Disetujui oleh", "Diterima oleh", "Diserahkan oleh"],
                &["Received"]),
            ("reversal", SlipInput { kind: SlipKind::CashOut, reversal_of: Some("JKT01-R2-K-261119-01"), copy: false, ..base },
                ReceiptWidth::Cols42, &english,
                &["*** REVERSAL ***", "CASH OUT", "Reversal of: JKT01-R2-K-261119-01"],
                &["COPY"]),
            ("cash in", SlipInput { kind: CashKind::In.into(), copy: false, approved_by: None, ..base },
                ReceiptWidth::Cols32, &english, &["*** CASH IN ***"], &["Approved"]),
            ("payout", SlipInput { kind: SlipKind::Payout, customer: Some("Budi S. · 0812****789"),
                balance_after: Some(m(0)), copy: false, ..base },
                ReceiptWidth::Cols32, &english,
                &["*** BALANCE PAYOUT ***", "Customer: Budi S.", "Balance"], &["COPY"]),
        ];
        for (name, input, width, label, present, absent) in cases {
            let p = build_slip(&input, &opts(width, label, &tender_name));
            let text = p.text();
            assert_fits(&p, width.cols());
            for want in present {
                assert!(text.contains(want), "{name}: missing {want:?} in\n{text}");
            }
            for not in absent {
                assert!(!text.contains(not), "{name}: {not:?} in\n{text}");
            }
            assert_eq!(p.large.len(), 1, "{name}");
            assert!(p.lines[p.large[0]].contains("Rp 300.000"), "{name}");
        }
    }

    fn figures_case1() -> SessionFigures {
        let c = case1();
        let mut events = c.events.clone();
        events.extend([
            SessionEvent::CashRounding(m(50)),
            SessionEvent::LineVoid(m(5_000)),
            SessionEvent::LineDiscount(m(2_000)),
            SessionEvent::Tender {
                key: TenderKey {
                    kind: TenderKind::CustomerBalance,
                    tender_type: None,
                },
                amount: m(1_000),
                cash_received: None,
                change_given: m(0),
                change_saved: m(0),
            },
        ]);
        let mut movements = c.movements.clone();
        movements.push(crate::cash::figures::tests::rev(&c.movements[1]));
        let photos = BTreeMap::new();
        session_figures(&SessionInput {
            opening_cash: m(200_000),
            events: &events,
            movements: &movements,
            reasons: &c.reasons,
            photos: &photos,
            nota_flag_above: m(100_000),
            drawer_opens: 2,
            x_reports: 1,
        })
        .unwrap()
    }

    use crate::sale::SessionEvent;

    #[test]
    fn x_report_text() {
        let f = figures_case1();
        let x = x_report(f, Some(m(250_000)), at(9)).unwrap();
        let session = SessionHeader {
            number: "JKT01-R2-S-261119-01",
            register: "R2",
            cashier: "Budi",
            opened_at: at(1),
            closed_at: None,
            forced: false,
            approved_by: Some("Sari"),
        };
        let input = XReportInput {
            header: HEADER,
            session,
            report: &x,
        };
        for (width, label, title) in [
            (
                ReceiptWidth::Cols32,
                &english as &dyn Fn(CashLabel) -> String,
                "X REPORT — not a close",
            ),
            (
                ReceiptWidth::Cols48,
                &indonesian,
                "LAPORAN X — bukan tutup kas",
            ),
        ] {
            let p = build_x_report(&input, &opts(width, label, &tender_name));
            assert_fits(&p, width.cols());
            let text = p.text();
            assert!(text.contains(title), "{text}");
            for absent in ["Counted", "Over / short", "Kas dihitung", "Selisih  "] {
                assert!(!text.contains(absent), "{absent:?} in\n{text}");
            }
        }
        let p = build_x_report(&input, &opts(ReceiptWidth::Cols42, &english, &tender_name));
        let text = p.text();
        // Expected: 353 500 + 30 000 (the cash out reversed).
        for want in [
            "Read at: 19/11/2026 16:00",
            "Opened: 19/11/2026 08:00",
            "Approved by: Sari",
            "EXPECTED CASH",
            "383.500",
            "Cash in (1)",
            "  Tambahan modal (1)",
            "Cash out (1)",
            "  Belanja (1, Reversals 1)",
            "Safe drops (1)",
            "-300.000",
            "Balance payouts",
            "-10.000",
            "  Tunai",
            "    Sales (2)",
            "    Refunds (1)",
            "  customer_balance",
            "Rounding",
            "Line voids (1)",
            "Line discounts (1)",
            "Reversals (1)",
            "Paid with balance",
            "Drawer opens",
            "X reads",
            "Missing nota",
            "Proposed opening",
            "Float difference",
            "-50.000",
        ] {
            assert!(text.contains(want), "missing {want:?} in\n{text}");
        }
        assert!(!text.contains("Saved change"));
        assert!(p.large.is_empty());
    }

    #[test]
    fn close_report_text() {
        let close = close_figures(figures_case1(), m(380_000), None, None).unwrap();
        let session = SessionHeader {
            number: "JKT01-R2-S-261119-01",
            register: "R2",
            cashier: "Budi",
            opened_at: at(1),
            closed_at: Some(at(10)),
            forced: true,
            approved_by: Some("Sari"),
        };
        let input = CloseReportInput {
            header: HEADER,
            session,
            report: &close,
            copy: true,
        };
        let p = build_close_report(&input, &opts(ReceiptWidth::Cols32, &english, &tender_name));
        assert_fits(&p, 32);
        let text = p.text();
        for want in [
            "*** COPY ***",
            "*** CLOSE REPORT ***",
            "*** FORCED CLOSE ***",
            "Closed: 19/11/2026 17:00",
            "Counted cash",
            "380.000",
            "Over / short",
            "-3.500",
            "Kept as float",
            "To the safe",
            "180.000",
        ] {
            assert!(text.contains(want), "missing {want:?} in\n{text}");
        }
        // No proposal and no difference: no float difference line.
        assert!(!text.contains("Float difference"));
        let plain = CloseReportInput {
            copy: false,
            session: SessionHeader {
                forced: false,
                approved_by: None,
                ..session
            },
            ..input
        };
        let p = build_close_report(
            &plain,
            &opts(ReceiptWidth::Cols48, &indonesian, &tender_name),
        );
        let text = p.text();
        assert!(text.contains("LAPORAN TUTUP KAS"));
        for absent in ["SALINAN", "TUTUP PAKSA", "Disetujui"] {
            assert!(!text.contains(absent), "{absent:?} in\n{text}");
        }
    }

    #[test]
    fn z_report_text() {
        let s = scripted();
        let day = business_day(at(3), BranchTimeZone::default());
        let z = build_z("JKT01-Z-261119-02", day, 2, at(15), &sessions(&s)).unwrap();
        let input = ZReportInput {
            header: HEADER,
            report: &z,
            run_by: "Owner",
            approved_by: Some("Sari"),
            copy: true,
        };
        for (width, label) in [
            (
                ReceiptWidth::Cols32,
                &english as &dyn Fn(CashLabel) -> String,
            ),
            (ReceiptWidth::Cols42, &indonesian),
            (ReceiptWidth::Cols48, &english),
        ] {
            assert_fits(
                &build_z_report(&input, &opts(width, label, &tender_name)),
                width.cols(),
            );
        }
        let p = build_z_report(&input, &opts(ReceiptWidth::Cols48, &english, &tender_name));
        let text = p.text();
        for want in [
            "*** COPY ***",
            "*** Z REPORT ***",
            "*** SUPPLEMENTARY ***",
            "JKT01-Z-261119-02",
            "Business day: 19/11/2026",
            "Run at: 19/11/2026 22:00",
            "Run by: Owner",
            "Approved by: Sari",
            "SESSIONS (3)",
            "JKT01-R1-S-261119-01",
            "  Register R2 · Andi",
            "  19/11 09:00 - 19/11 21:00",
            "  (FORCED CLOSE)",
            "  Float difference",
            "== Register R1 ==",
            "== Register R2 ==",
            "== BRANCH TOTAL ==",
            "First sale: JKT01-R1-261119-0001",
            "Last sale: JKT01-R1-261119-0003",
            "DPP 11%",
            "PPN 11%",
            "43.950",
            "DPP 0%",
            "Gross sales",
            "Net sales",
            "CASH TO DEPOSIT",
        ] {
            assert!(text.contains(want), "missing {want:?} in\n{text}");
        }
        assert_eq!(p.large.len(), 1);
        assert_eq!(p.lines[p.large[0]].trim(), "Rp 653.000");
        // The first Z of a day, not a copy, without an approver.
        let z1 = build_z("JKT01-Z-261119-01", day, 1, at(15), &sessions(&s)).unwrap();
        let plain = ZReportInput {
            report: &z1,
            approved_by: None,
            copy: false,
            ..input
        };
        let text = build_z_report(
            &plain,
            &opts(ReceiptWidth::Cols32, &indonesian, &tender_name),
        )
        .text();
        assert!(text.contains("LAPORAN Z"));
        assert!(text.contains("KAS UNTUK DISETOR"));
        for absent in ["SUSULAN", "SALINAN", "Disetujui"] {
            assert!(!text.contains(absent), "{absent:?} in\n{text}");
        }
    }

    #[test]
    fn every_label_has_both_texts() {
        for l in CashLabel::ALL {
            assert!(!l.english().is_empty());
            assert!(!l.indonesian().is_empty());
            assert_eq!(l.text("en"), l.english());
            assert_eq!(l.text("id-ID"), l.indonesian());
        }
        let mut seen: Vec<_> = CashLabel::ALL.iter().map(|l| format!("{l:?}")).collect();
        seen.sort();
        seen.dedup();
        assert_eq!(seen.len(), CashLabel::ALL.len());
        assert_eq!(CashLabel::XReport.text("en"), "X REPORT — not a close");
        assert_eq!(CashLabel::ReceivedBy.text("id"), "Diterima oleh");
        assert_eq!(CashLabel::GivenBy.text("id"), "Diserahkan oleh");
    }
}
