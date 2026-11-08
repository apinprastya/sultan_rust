//! The fixed permission catalog and limits (spec 2.1 §3).

use std::fmt;
use std::str::FromStr;

use crate::money::Money;

/// 100% in basis points.
pub const FULL_PCT_BASIS_POINTS: u16 = 10_000;

/// A permission from the fixed catalog (spec 2.1 §3).
///
/// Each permission has a stable string key ([`Permission::as_str`]) used in the database, the API
/// and role seeds. Keys must never change once released; adding a permission is a code change plus
/// a migration of the seeded roles.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Permission {
    /// `sale.create`: sell at the register.
    SaleCreate,
    /// `sale.discount`: line discount, limited by [`Limit::MaxPct`].
    SaleDiscount,
    /// `sale.cart_discount`: whole-cart discount, limited by [`Limit::MaxPct`].
    SaleCartDiscount,
    /// `sale.void_line`: void a line of an open sale.
    SaleVoidLine,
    /// `sale.void`: void an open sale.
    SaleVoid,
    /// `sale.post_void`: void a completed sale.
    SalePostVoid,
    /// `price.override`: manual price at the till.
    PriceOverride,
    /// `refund.with_receipt`: return by receipt lookup.
    RefundWithReceipt,
    /// `refund.no_receipt`: return without receipt, limited by [`Limit::MaxAmount`].
    RefundNoReceipt,
    /// `cash.in`: petty cash into the drawer.
    CashIn,
    /// `cash.out`: petty cash out of the drawer, limited by [`Limit::MaxAmount`].
    CashOut,
    /// `drawer.open`: open the drawer without a sale.
    DrawerOpen,
    /// `session.open_close`: open and close one's own cashier session.
    SessionOpenClose,
    /// `session.view_expected`: see expected cash before or while counting.
    SessionViewExpected,
    /// `session.force_close`: close someone else's abandoned session.
    SessionForceClose,
    /// `user.manage`: manage users (HQ only).
    UserManage,
    /// `role.manage`: manage roles (HQ only).
    RoleManage,
    /// `org.manage`: branches, registers, locations, pairing (HQ only).
    OrgManage,
    /// `user.unlock`: unlock a locked-out user.
    UserUnlock,
    /// `stock.adjust`: stock adjustments.
    StockAdjust,
    /// `report.sales`: sales reports.
    ReportSales,
    /// `report.profit`: profit reports.
    ReportProfit,
    /// `audit.view`: audit log viewer.
    AuditView,
    /// `catalog.manage`: products, variants, units, barcodes, categories, brands, assortment,
    /// generator settings and scale layouts (HQ only, spec 2.2 §11).
    CatalogManage,
    /// `catalog.view_archived`: list and open archived catalog entries (spec 2.2 §11).
    CatalogViewArchived,
}

impl Permission {
    /// Every permission in the catalog, in catalog order.
    pub const ALL: [Self; 25] = [
        Self::SaleCreate,
        Self::SaleDiscount,
        Self::SaleCartDiscount,
        Self::SaleVoidLine,
        Self::SaleVoid,
        Self::SalePostVoid,
        Self::PriceOverride,
        Self::RefundWithReceipt,
        Self::RefundNoReceipt,
        Self::CashIn,
        Self::CashOut,
        Self::DrawerOpen,
        Self::SessionOpenClose,
        Self::SessionViewExpected,
        Self::SessionForceClose,
        Self::UserManage,
        Self::RoleManage,
        Self::OrgManage,
        Self::UserUnlock,
        Self::StockAdjust,
        Self::ReportSales,
        Self::ReportProfit,
        Self::AuditView,
        Self::CatalogManage,
        Self::CatalogViewArchived,
    ];

    /// The stable string key, e.g. `"sale.discount"`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SaleCreate => "sale.create",
            Self::SaleDiscount => "sale.discount",
            Self::SaleCartDiscount => "sale.cart_discount",
            Self::SaleVoidLine => "sale.void_line",
            Self::SaleVoid => "sale.void",
            Self::SalePostVoid => "sale.post_void",
            Self::PriceOverride => "price.override",
            Self::RefundWithReceipt => "refund.with_receipt",
            Self::RefundNoReceipt => "refund.no_receipt",
            Self::CashIn => "cash.in",
            Self::CashOut => "cash.out",
            Self::DrawerOpen => "drawer.open",
            Self::SessionOpenClose => "session.open_close",
            Self::SessionViewExpected => "session.view_expected",
            Self::SessionForceClose => "session.force_close",
            Self::UserManage => "user.manage",
            Self::RoleManage => "role.manage",
            Self::OrgManage => "org.manage",
            Self::UserUnlock => "user.unlock",
            Self::StockAdjust => "stock.adjust",
            Self::ReportSales => "report.sales",
            Self::ReportProfit => "report.profit",
            Self::AuditView => "audit.view",
            Self::CatalogManage => "catalog.manage",
            Self::CatalogViewArchived => "catalog.view_archived",
        }
    }

    /// The kind of limit this permission takes, or `None` if it is all-or-nothing (spec 2.1 §3).
    #[must_use]
    pub const fn limit_kind(self) -> Option<LimitKind> {
        match self {
            Self::SaleDiscount | Self::SaleCartDiscount => Some(LimitKind::Pct),
            Self::RefundNoReceipt | Self::CashOut => Some(LimitKind::Amount),
            _ => None,
        }
    }

    /// Whether a manager can approve this action for a user who lacks the permission
    /// (spec 2.1 §6).
    ///
    /// Overridable permissions are the till actions a cashier performs in front of a customer:
    /// discounts, voids, price overrides, refunds, cash in/out and opening the drawer. Back-office
    /// permissions (users, roles, org, reports, audit, stock), session permissions and
    /// `sale.create` are not: a manager does those under their own login.
    #[must_use]
    pub const fn is_overridable(self) -> bool {
        matches!(
            self,
            Self::SaleDiscount
                | Self::SaleCartDiscount
                | Self::SaleVoidLine
                | Self::SaleVoid
                | Self::SalePostVoid
                | Self::PriceOverride
                | Self::RefundWithReceipt
                | Self::RefundNoReceipt
                | Self::CashIn
                | Self::CashOut
                | Self::DrawerOpen
        )
    }
}

impl fmt::Display for Permission {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A string that is not a key of the permission catalog.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown permission `{0}`")]
pub struct UnknownPermission(pub String);

impl FromStr for Permission {
    type Err = UnknownPermission;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|p| p.as_str() == s)
            .ok_or_else(|| UnknownPermission(s.to_owned()))
    }
}

/// Which kind of value a limited permission is measured in (spec 2.1 §3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LimitKind {
    /// A percentage in basis points, see [`Limit::MaxPct`].
    Pct,
    /// A money amount, see [`Limit::MaxAmount`].
    Amount,
}

/// The ceiling a role grants on a limited permission (spec 2.1 §3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Limit {
    /// Maximum percentage in basis points (`1_000` = 10%).
    MaxPct(u16),
    /// Maximum money amount.
    MaxAmount(Money),
}

impl Limit {
    /// The kind of this limit.
    #[must_use]
    pub const fn kind(self) -> LimitKind {
        match self {
            Self::MaxPct(_) => LimitKind::Pct,
            Self::MaxAmount(_) => LimitKind::Amount,
        }
    }

    /// Whether `requested` is within this limit. The boundary is inclusive: a 10% limit allows
    /// exactly 10%. A value of another kind is never covered.
    #[must_use]
    pub fn covers(self, requested: Requested) -> bool {
        match (self, requested) {
            (Self::MaxPct(max), Requested::Pct(value)) => value <= max,
            (Self::MaxAmount(max), Requested::Amount(value)) => value <= max,
            _ => false,
        }
    }

    /// The wider of two limits of the same kind ("the highest limit wins", spec 2.1 §3).
    /// Grants are validated against the permission's limit kind, so mixed kinds do not occur;
    /// if they did, `self` is kept.
    #[must_use]
    pub(crate) fn wider(self, other: Self) -> Self {
        match (self, other) {
            (Self::MaxPct(a), Self::MaxPct(b)) => Self::MaxPct(a.max(b)),
            (Self::MaxAmount(a), Self::MaxAmount(b)) => Self::MaxAmount(a.max(b)),
            _ => self,
        }
    }
}

impl fmt::Display for Limit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MaxPct(bp) => write!(f, "max {}", Pct(*bp)),
            Self::MaxAmount(amount) => write!(f, "max {amount}"),
        }
    }
}

/// The value an action asks for, compared against a [`Limit`] (e.g. a 15% discount).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Requested {
    /// A percentage in basis points (`1_500` = 15%).
    Pct(u16),
    /// A money amount.
    Amount(Money),
}

impl Requested {
    /// The kind of this value.
    #[must_use]
    pub const fn kind(self) -> LimitKind {
        match self {
            Self::Pct(_) => LimitKind::Pct,
            Self::Amount(_) => LimitKind::Amount,
        }
    }
}

impl fmt::Display for Requested {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Pct(bp) => Pct(*bp).fmt(f),
            Self::Amount(amount) => amount.fmt(f),
        }
    }
}

/// Formats basis points as a percentage with two decimals.
struct Pct(u16);

impl fmt::Display for Pct {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{:02}%", self.0 / 100, self.0 % 100)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_key_round_trips() {
        for p in Permission::ALL {
            assert_eq!(p.as_str().parse::<Permission>(), Ok(p));
            assert_eq!(p.to_string(), p.as_str());
        }
    }

    #[test]
    fn keys_are_unique() {
        let mut keys: Vec<_> = Permission::ALL.iter().map(|p| p.as_str()).collect();
        keys.sort_unstable();
        keys.dedup();
        assert_eq!(keys.len(), Permission::ALL.len());
    }

    #[test]
    fn unknown_key_is_rejected() {
        for bad in ["", "sale", "SALE.CREATE", "sale.discount ", "sale.refund"] {
            let err = bad.parse::<Permission>().unwrap_err();
            assert_eq!(err, UnknownPermission(bad.to_owned()));
        }
        assert_eq!(
            UnknownPermission("x".into()).to_string(),
            "unknown permission `x`"
        );
    }

    #[test]
    fn limit_kinds_match_the_spec_table() {
        let limited: Vec<_> = Permission::ALL
            .into_iter()
            .filter_map(|p| p.limit_kind().map(|k| (p.as_str(), k)))
            .collect();
        assert_eq!(
            limited,
            [
                ("sale.discount", LimitKind::Pct),
                ("sale.cart_discount", LimitKind::Pct),
                ("refund.no_receipt", LimitKind::Amount),
                ("cash.out", LimitKind::Amount),
            ]
        );
    }

    #[test]
    fn overridable_is_only_till_actions() {
        assert!(Permission::SaleDiscount.is_overridable());
        assert!(Permission::DrawerOpen.is_overridable());
        assert!(!Permission::SaleCreate.is_overridable());
        assert!(!Permission::UserManage.is_overridable());
        assert!(!Permission::SessionViewExpected.is_overridable());
        assert!(!Permission::CatalogManage.is_overridable());
        assert!(!Permission::CatalogViewArchived.is_overridable());
    }

    #[test]
    fn catalog_keys_are_stable() {
        assert_eq!(Permission::CatalogManage.as_str(), "catalog.manage");
        assert_eq!(
            "catalog.view_archived".parse::<Permission>(),
            Ok(Permission::CatalogViewArchived)
        );
        assert_eq!(Permission::CatalogManage.limit_kind(), None);
        assert_eq!(Permission::ALL.len(), 25);
    }

    #[test]
    fn covers_is_inclusive_and_kind_strict() {
        let pct = Limit::MaxPct(1_000);
        let amount = Limit::MaxAmount(Money::from_minor(50_000));
        let cases = [
            (pct, Requested::Pct(999), true),
            (pct, Requested::Pct(1_000), true),
            (pct, Requested::Pct(1_001), false),
            (amount, Requested::Amount(Money::from_minor(50_000)), true),
            (amount, Requested::Amount(Money::from_minor(50_001)), false),
            (pct, Requested::Amount(Money::ZERO), false),
            (amount, Requested::Pct(0), false),
        ];
        for (limit, requested, expected) in cases {
            assert_eq!(limit.covers(requested), expected, "{limit} vs {requested}");
        }
    }

    #[test]
    fn wider_takes_the_highest() {
        let five = Limit::MaxPct(500);
        let ten = Limit::MaxPct(1_000);
        assert_eq!(five.wider(ten), ten);
        assert_eq!(ten.wider(five), ten);
        let small = Limit::MaxAmount(Money::from_minor(1));
        let big = Limit::MaxAmount(Money::from_minor(2));
        assert_eq!(small.wider(big), big);
        assert_eq!(five.wider(big), five, "mixed kinds keep self");
    }

    #[test]
    fn kinds_and_display() {
        assert_eq!(Limit::MaxPct(0).kind(), LimitKind::Pct);
        assert_eq!(Limit::MaxAmount(Money::ZERO).kind(), LimitKind::Amount);
        assert_eq!(Requested::Pct(0).kind(), LimitKind::Pct);
        assert_eq!(Requested::Amount(Money::ZERO).kind(), LimitKind::Amount);
        assert_eq!(Limit::MaxPct(1_050).to_string(), "max 10.50%");
        assert_eq!(Requested::Pct(5).to_string(), "0.05%");
        assert_eq!(Requested::Amount(Money::from_minor(7)).to_string(), "7");
    }
}
