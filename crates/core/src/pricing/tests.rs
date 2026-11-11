//! Spec 2.3 §12 test cases 1–12, 17 and 18 (13 is in `override_`, 19 in `rounding`, 14–16 in
//! `crate::tax`), plus validation and edge cases.

use chrono::NaiveDate;
use uuid::Uuid;

use super::*;
use crate::catalog::Qty;
use crate::doc_number::BranchTimeZone;
use crate::ids::{UnitId, VariantId};
use crate::money::Money;
use crate::time::Timestamp;

const V: VariantId = VariantId::from_uuid(Uuid::from_u128(1));
const W: VariantId = VariantId::from_uuid(Uuid::from_u128(2));
const PCS: UnitId = UnitId::from_uuid(Uuid::from_u128(10));
const PACK: UnitId = UnitId::from_uuid(Uuid::from_u128(11));
const BOX: UnitId = UnitId::from_uuid(Uuid::from_u128(12));

fn m(v: i64) -> Money {
    Money::from_minor(v)
}

fn q(whole: i64) -> Qty {
    Qty::from_whole(whole).unwrap()
}

fn d(y: i32, mo: u32, day: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(y, mo, day).unwrap()
}

fn today() -> NaiveDate {
    d(2026, 11, 11)
}

fn fixed(min: i64, price: i64) -> PriceTier {
    PriceTier {
        min_qty: q(min),
        price: TierPrice::Fixed(m(price)),
    }
}

fn ruled(min: i64, rule: Rule) -> PriceTier {
    PriceTier {
        min_qty: q(min),
        price: TierPrice::Rule(rule),
    }
}

fn entry(variant: VariantId, unit: UnitId, tiers: Vec<PriceTier>) -> PriceEntry {
    PriceEntry {
        id: PriceEntryId::generate(),
        variant,
        unit,
        valid_from: None,
        valid_to: None,
        tiers,
    }
}

fn dated(
    unit: UnitId,
    from: Option<NaiveDate>,
    to: Option<NaiveDate>,
    tiers: Vec<PriceTier>,
) -> PriceEntry {
    PriceEntry {
        valid_from: from,
        valid_to: to,
        ..entry(V, unit, tiers)
    }
}

fn list(kind: PriceListKind, rule: Option<Rule>, entries: Vec<PriceEntry>) -> PriceList {
    PriceList {
        rule,
        entries,
        ..PriceList::new(PriceListId::generate(), kind)
    }
}

/// The default list: V pcs 10 000.
fn default_list() -> PriceList {
    list(
        PriceListKind::Default,
        None,
        vec![entry(V, PCS, vec![fixed(1, 10_000)])],
    )
}

fn item(unit: UnitId) -> SaleItem {
    let factor = match unit {
        u if u == PACK => q(6),
        u if u == BOX => q(24),
        _ => Qty::ONE,
    };
    SaleItem {
        variant: V,
        unit,
        base_unit: PCS,
        factor,
    }
}

fn line(unit: UnitId, qty: i64) -> CartLine {
    CartLine {
        item: item(unit),
        qty: q(qty),
        mode: LineMode::Listed,
    }
}

/// The unit price of one line `unit × qty` alone in the cart.
fn price_one(chain: &PriceChain<'_>, unit: UnitId, qty: i64) -> Result<Money, PricingError> {
    resolve_cart(chain, &[line(unit, qty)])
        .remove(0)
        .map(|l| l.unit_price)
}

fn gold(applies: bool) -> TierMembership {
    TierMembership {
        tier_archived: false,
        valid_until: (!applies).then(|| d(2026, 1, 1)),
    }
}

/// §12.1 and §12.17.
#[test]
fn default_only_with_quantity_tier() {
    let default = list(
        PriceListKind::Default,
        None,
        vec![entry(V, PCS, vec![fixed(12, 9_000), fixed(1, 10_000)])],
    );
    let chain = PriceChain::store(None, &default, today());
    let cases = [
        (1, 10_000, 10_000, 1),
        (11, 10_000, 110_000, 1),
        (15, 9_000, 135_000, 12),
    ];
    for (qty, unit_price, amount, tier) in cases {
        let got = resolve_cart(&chain, &[line(PCS, qty)]).remove(0).unwrap();
        assert_eq!(got.unit_price, m(unit_price), "qty {qty}");
        assert_eq!(got.amount, m(amount), "qty {qty}");
        let source = got.resolved.unwrap().source;
        assert_eq!(source.list, default.id);
        assert_eq!(source.kind, PriceListKind::Default);
        assert_eq!(source.tier_min_qty, Some(q(tier)));
        assert!(!source.unit_fallback);
    }

    // §12.17: 1.255 kg × 12 990 → 16 302.
    let kg = list(
        PriceListKind::Default,
        None,
        vec![entry(V, PCS, vec![fixed(1, 12_990)])],
    );
    let chain = PriceChain::store(None, &kg, today());
    let mut weighed = line(PCS, 0);
    weighed.qty = Qty::from_thousandths(1_255);
    let got = resolve_cart(&chain, &[weighed]).remove(0).unwrap();
    assert_eq!(got.amount, m(16_302));
}

/// §12.2: tiers count the variant over the whole cart in base units.
#[test]
fn tiers_count_the_whole_cart() {
    let default = list(
        PriceListKind::Default,
        None,
        vec![entry(V, PCS, vec![fixed(1, 10_000), fixed(12, 9_000)])],
    );
    let chain = PriceChain::store(None, &default, today());
    let cart = [line(PCS, 10), line(PACK, 1)];
    assert_eq!(counted_quantities(&cart)[&V], Some(q(16)));
    let priced: Vec<_> = resolve_cart(&chain, &cart)
        .into_iter()
        .map(|l| l.unwrap())
        .collect();
    assert_eq!(priced[0].unit_price, m(9_000));
    assert_eq!(priced[0].amount, m(90_000));
    assert_eq!(priced[1].unit_price, m(54_000)); // 6 × 9 000, unit fallback
    assert!(priced[1].resolved.unwrap().source.unit_fallback);

    // Remove the pack → Q = 10 → back to 10 000.
    assert_eq!(price_one(&chain, PCS, 10), Ok(m(10_000)));

    // min_qty is in the entry's unit: from 2 box(24) = 48 pcs.
    let default = list(
        PriceListKind::Default,
        None,
        vec![
            entry(V, PCS, vec![fixed(1, 10_000)]),
            entry(V, BOX, vec![fixed(1, 230_000), fixed(2, 220_000)]),
        ],
    );
    let chain = PriceChain::store(None, &default, today());
    let at = |cart: &[CartLine]| {
        resolve_cart(&chain, cart)
            .into_iter()
            .map(|l| l.unwrap().unit_price)
            .collect::<Vec<_>>()
    };
    assert_eq!(at(&[line(BOX, 1), line(PCS, 23)]), [m(230_000), m(10_000)]);
    assert_eq!(at(&[line(BOX, 1), line(PCS, 24)]), [m(220_000), m(10_000)]);
    assert_eq!(at(&[line(BOX, 2)]), [m(220_000)]);
}

/// §12.3: contract beats tier beats group beats branch beats default.
#[test]
fn chain_order() {
    let lists = [
        list(
            PriceListKind::Customer,
            None,
            vec![entry(V, PCS, vec![fixed(1, 7_000)])],
        ),
        list(
            PriceListKind::Tier,
            None,
            vec![entry(V, PCS, vec![fixed(1, 8_000)])],
        ),
        list(
            PriceListKind::Group,
            None,
            vec![entry(V, PCS, vec![fixed(1, 8_500)])],
        ),
        list(
            PriceListKind::Branch,
            None,
            vec![entry(V, PCS, vec![fixed(1, 9_000)])],
        ),
    ];
    let default = default_list();
    let [contract, tier, group, branch] = &lists;
    // (contract, tier, group, branch) present → expected price and kind.
    #[rustfmt::skip]
    let cases = [
        ([true, true, true, true], 7_000, PriceListKind::Customer),
        ([false, true, true, true], 8_000, PriceListKind::Tier),
        ([false, false, true, true], 8_500, PriceListKind::Group),
        ([false, false, false, true], 9_000, PriceListKind::Branch),
        ([false, false, false, false], 10_000, PriceListKind::Default),
        ([true, false, false, false], 7_000, PriceListKind::Customer),
        ([false, true, false, true], 8_000, PriceListKind::Tier),
    ];
    for ([c, t, g, b], price, kind) in cases {
        let customer = CustomerLists {
            contract: c.then_some(contract),
            tier: t.then_some((tier, gold(true))),
            group: g.then_some(group),
        };
        let chain = PriceChain::build(Some(&customer), b.then_some(branch), &default, today());
        let got = resolve_unit_price(&chain, &item(PCS), Qty::ONE)
            .unwrap()
            .unwrap();
        assert_eq!(got.unit_price, m(price), "{c} {t} {g} {b}");
        assert_eq!(got.source.kind, kind);
    }

    // Inactive and archived lists are skipped.
    let mut off = contract.clone();
    off.active = false;
    let mut archived = group.clone();
    archived.archived = true;
    let customer = CustomerLists {
        contract: Some(&off),
        tier: None,
        group: Some(&archived),
    };
    let chain = PriceChain::build(Some(&customer), Some(branch), &default, today());
    assert_eq!(chain.lists().len(), 2);
    assert_eq!(chain.date(), today());
    assert_eq!(price_one(&chain, PCS, 1), Ok(m(9_000)));
    let chain = PriceChain::build(Some(&CustomerLists::default()), None, &default, today());
    assert_eq!(chain.lists().len(), 1);
}

/// §12.4: a list without a matching tier falls through.
#[test]
fn fall_through() {
    let wholesale = list(
        PriceListKind::Group,
        None,
        vec![entry(V, PCS, vec![fixed(12, 8_500)])],
    );
    let default = default_list();
    let customer = CustomerLists {
        group: Some(&wholesale),
        ..CustomerLists::default()
    };
    let chain = PriceChain::build(Some(&customer), None, &default, today());
    for (qty, price) in [(5, 10_000), (11, 10_000), (12, 8_500), (40, 8_500)] {
        assert_eq!(price_one(&chain, PCS, qty), Ok(m(price)), "qty {qty}");
    }
}

/// §12.5, §12.6, §12.8: rules apply to the next applicable price.
#[test]
fn rules_on_the_next_price() {
    let default = default_list();
    let bali = list(
        PriceListKind::Branch,
        None,
        vec![entry(V, PCS, vec![fixed(1, 11_000)])],
    );
    let gold_list = |rule, entries| list(PriceListKind::Tier, Some(rule), entries);
    let pct5 = gold_list(Rule::Pct(-500), vec![]);
    let minus500 = gold_list(Rule::Amount(m(-500)), vec![]);
    let too_much = gold_list(Rule::Amount(m(-10_000)), vec![]);
    let almost_all = gold_list(Rule::Pct(-9_999), vec![]);
    let plus = gold_list(Rule::Amount(m(250)), vec![]);
    // §12.8: an entry rule overrides the list rule.
    let entry_rule = gold_list(
        Rule::Pct(-500),
        vec![entry(V, PCS, vec![ruled(1, Rule::Pct(-1_000))])],
    );
    let entry_rule_too_much = gold_list(
        Rule::Pct(-500),
        vec![entry(V, PCS, vec![ruled(1, Rule::Amount(m(-20_000)))])],
    );
    #[rustfmt::skip]
    let cases: [(&PriceList, bool, i64, Option<Qty>); 9] = [
        (&pct5, true, 10_450, None),         // Bali 11 000 −5%
        (&pct5, false, 9_500, None),         // elsewhere 10 000 −5%
        (&minus500, false, 9_500, None),     // −Rp 500
        (&too_much, false, 10_000, Some(q(1))),    // ≤ 0 → no price from Gold
        (&almost_all, false, 1, None),        // −99.99% of 10 000 = 1
        (&plus, true, 11_250, None),
        (&entry_rule, true, 9_900, Some(q(1))),
        (&entry_rule, false, 9_000, Some(q(1))),
        (&entry_rule_too_much, false, 10_000, Some(q(1))),
    ];
    for (gold_list, at_bali, price, tier) in cases {
        let customer = CustomerLists {
            tier: Some((gold_list, gold(true))),
            ..CustomerLists::default()
        };
        let chain = PriceChain::build(Some(&customer), at_bali.then_some(&bali), &default, today());
        let got = resolve_unit_price(&chain, &item(PCS), Qty::ONE)
            .unwrap()
            .unwrap();
        assert_eq!(
            got.unit_price,
            m(price),
            "{:?} bali={at_bali}",
            gold_list.rule
        );
        assert_eq!(got.source.tier_min_qty, tier);
        let kind = if price == 10_000 {
            PriceListKind::Default
        } else if price == 11_000 {
            PriceListKind::Branch
        } else {
            PriceListKind::Tier
        };
        assert_eq!(got.source.kind, kind);
    }

    // §12.8: the list rule still prices items without an entry.
    let default = list(
        PriceListKind::Default,
        None,
        vec![
            entry(V, PCS, vec![fixed(1, 10_000)]),
            entry(W, PCS, vec![fixed(1, 20_000)]),
        ],
    );
    let customer = CustomerLists {
        tier: Some((&entry_rule, gold(true))),
        ..CustomerLists::default()
    };
    let chain = PriceChain::build(Some(&customer), None, &default, today());
    let w = SaleItem {
        variant: W,
        ..item(PCS)
    };
    let got = resolve_unit_price(&chain, &w, Qty::ONE).unwrap().unwrap();
    assert_eq!(got.unit_price, m(19_000));
}

/// §12.7 and the rule arithmetic.
#[test]
fn rule_arithmetic() {
    #[rustfmt::skip]
    let cases = [
        (Rule::Pct(-500), 10_450, Ok(Some(9_928))), // 9 927.5 → 9 928
        (Rule::Pct(-1_000), 333, Ok(Some(300))),    // 299.7 → 300
        (Rule::Pct(-500), 11_000, Ok(Some(10_450))),
        (Rule::Pct(1_000), 10_000, Ok(Some(11_000))),
        (Rule::Pct(0), 10_000, Ok(Some(10_000))),
        (Rule::Pct(-10_000), 10_000, Ok(None)),
        (Rule::Pct(-9_999), 1, Ok(None)),
        (Rule::Amount(m(-500)), 10_000, Ok(Some(9_500))),
        (Rule::Amount(m(-10_000)), 10_000, Ok(None)),
        (Rule::Amount(m(500)), 0, Ok(Some(500))),
        (Rule::Amount(m(i64::MAX)), 1, Err(PricingError::Overflow)),
        (Rule::Pct(10_000), i64::MAX, Err(PricingError::Overflow)),
    ];
    for (rule, base, expected) in cases {
        assert_eq!(
            rule.apply(m(base)),
            expected.map(|o| o.map(m)),
            "{rule:?} on {base}"
        );
    }
}

/// (tier list, branch list, box price, unit fallback).
type FallbackCase<'a> = (
    Option<&'a PriceList>,
    Option<&'a PriceList>,
    Option<i64>,
    bool,
);

/// §12.9: unit fallback.
#[test]
fn unit_fallback() {
    let default = default_list();
    let branch_box = list(
        PriceListKind::Branch,
        None,
        vec![entry(V, BOX, vec![fixed(1, 230_000)])],
    );
    let empty_branch = list(PriceListKind::Branch, None, vec![]);
    let gold_rule = list(PriceListKind::Tier, Some(Rule::Pct(-500)), vec![]);
    let gold_box_rule_only = list(
        PriceListKind::Tier,
        None,
        vec![entry(V, BOX, vec![ruled(1, Rule::Pct(-500))])],
    );
    #[rustfmt::skip]
    let cases: [FallbackCase<'_>; 6] = [
        (None, None, Some(240_000), true),                    // 24 × 10 000
        (None, Some(&empty_branch), Some(240_000), true),
        (None, Some(&branch_box), Some(230_000), false),      // the box price
        (Some(&gold_rule), None, Some(228_000), true),        // 24 × 9 500
        (Some(&gold_rule), Some(&branch_box), Some(218_500), false), // −5% of the box price
        // An entry tier exists for box, so no fallback, but it has no base: no price.
        (Some(&gold_box_rule_only), None, None, false),
    ];
    for (tier, branch, price, fallback) in cases {
        let customer = CustomerLists {
            tier: tier.map(|t| (t, gold(true))),
            ..CustomerLists::default()
        };
        let chain = PriceChain::build(Some(&customer), branch, &default, today());
        let got = resolve_unit_price(&chain, &item(BOX), q(24)).unwrap();
        assert_eq!(
            got.map(|r| r.unit_price),
            price.map(m),
            "{tier:?} {branch:?}"
        );
        if let Some(r) = got {
            assert_eq!(r.source.unit_fallback, fallback);
        }
    }

    // A decimal factor rounds half up: 0.5 kg-pack of 333/kg = 166.5 → 167.
    let half = SaleItem {
        factor: Qty::from_thousandths(500),
        ..item(PACK)
    };
    let cheap = list(
        PriceListKind::Default,
        None,
        vec![entry(V, PCS, vec![fixed(1, 333)])],
    );
    let chain = PriceChain::store(None, &cheap, today());
    let got = resolve_unit_price(&chain, &half, Qty::ONE)
        .unwrap()
        .unwrap();
    assert_eq!(got.unit_price, m(167));

    // The base price × factor can overflow.
    let huge = list(
        PriceListKind::Default,
        None,
        vec![entry(V, PCS, vec![fixed(1, i64::MAX)])],
    );
    let chain = PriceChain::store(None, &huge, today());
    assert_eq!(
        resolve_unit_price(&chain, &item(BOX), q(24)),
        Err(PricingError::Overflow)
    );
}

/// §12.10: no price, open price, rule without base; overridden lines.
#[test]
fn no_price_open_price_and_overrides() {
    let default = default_list();
    let gold_rule = list(PriceListKind::Tier, Some(Rule::Pct(-500)), vec![]);
    let customer = CustomerLists {
        tier: Some((&gold_rule, gold(true))),
        ..CustomerLists::default()
    };
    let chain = PriceChain::build(Some(&customer), None, &default, today());
    let w = |mode| CartLine {
        item: SaleItem {
            variant: W,
            ..item(PCS)
        },
        qty: q(2),
        mode,
    };
    let v = |mode| CartLine {
        mode,
        ..line(PCS, 2)
    };
    let no_price = |suggestion: Option<i64>| PricingError::NoPrice {
        suggestion: suggestion.map(m),
    };
    #[rustfmt::skip]
    let cases = [
        // W has no price anywhere; Gold's rule has no base → no_price.
        (w(LineMode::Listed), Err(no_price(None))),
        (w(LineMode::OpenPrice(None)), Err(no_price(None))),
        (w(LineMode::OpenPrice(Some(m(4_000)))), Ok((None, 4_000, 8_000))),
        (w(LineMode::Overridden(m(0))), Ok((None, 0, 0))),
        (v(LineMode::OpenPrice(None)), Err(no_price(Some(9_500)))),
        (v(LineMode::OpenPrice(Some(m(7_000)))), Ok((Some(9_500), 7_000, 14_000))),
        (v(LineMode::Overridden(m(8_000))), Ok((Some(9_500), 8_000, 16_000))),
        (v(LineMode::Overridden(m(-1))), Err(PricingError::InvalidField {
            field: "price", reason: "must not be negative" })),
    ];
    for (cart_line, expected) in cases {
        let got = resolve_cart(&chain, &[cart_line]).remove(0).map(|l| {
            (
                l.resolved.map(|r| r.unit_price.minor()),
                l.unit_price.minor(),
                l.amount.minor(),
            )
        });
        assert_eq!(got, expected, "{:?}", cart_line.mode);
    }
    assert_eq!(
        PricingError::NoPrice { suggestion: None }.code(),
        "no_price"
    );

    // An overridden line still counts towards the tier of the other lines.
    let tiered = list(
        PriceListKind::Default,
        None,
        vec![entry(V, PCS, vec![fixed(1, 10_000), fixed(12, 9_000)])],
    );
    let chain = PriceChain::store(None, &tiered, today());
    let cart = [v(LineMode::Overridden(m(5_000))), line(PCS, 10)];
    let got = resolve_cart(&chain, &cart);
    assert_eq!(got[1].as_ref().unwrap().unit_price, m(9_000));
}

/// §12.11: entry validity by the branch-local sale date.
#[test]
fn entry_validity() {
    let default = list(
        PriceListKind::Default,
        None,
        vec![
            dated(PCS, None, Some(d(2026, 11, 30)), vec![fixed(1, 10_000)]),
            dated(PCS, Some(d(2026, 12, 1)), None, vec![fixed(1, 11_000)]),
        ],
    );
    assert_eq!(default.validate(), Ok(()));
    let jakarta = BranchTimeZone::default();
    // 2026-11-30T16:30Z = 23:30 WIB on 30 Nov; 17:00Z = 00:00 WIB on 1 Dec.
    let nov30_1630z = Timestamp::from_unix_millis(1_796_056_200_000);
    let cases = [
        (nov30_1630z, d(2026, 11, 30), 10_000),
        (
            nov30_1630z.saturating_add_millis(1_800_000),
            d(2026, 12, 1),
            11_000,
        ),
    ];
    for (at, date, price) in cases {
        assert_eq!(sale_date(at, jakarta), date);
        let chain = PriceChain::store(None, &default, sale_date(at, jakarta));
        assert_eq!(price_one(&chain, PCS, 1), Ok(m(price)), "{date}");
    }

    // A list whose only entry is not valid yet falls through.
    let branch = list(
        PriceListKind::Branch,
        None,
        vec![dated(
            PCS,
            Some(d(2027, 1, 1)),
            Some(d(2027, 1, 31)),
            vec![fixed(1, 1)],
        )],
    );
    let default = default_list();
    let chain = PriceChain::store(Some(&branch), &default, today());
    assert_eq!(price_one(&chain, PCS, 1), Ok(m(10_000)));
}

/// §12.12 and spec 2.5 §4: the tier list applies only while the membership does.
#[test]
fn tier_membership_on_the_date() {
    let default = default_list();
    let gold_rule = list(PriceListKind::Tier, Some(Rule::Pct(-500)), vec![]);
    let until = |y, mo, day| TierMembership {
        tier_archived: false,
        valid_until: Some(d(y, mo, day)),
    };
    let cases = [
        (
            TierMembership {
                tier_archived: false,
                valid_until: None,
            },
            9_500,
        ),
        (until(2026, 12, 31), 9_500),
        (until(2026, 11, 11), 9_500),
        (until(2026, 11, 10), 10_000),
        (
            TierMembership {
                tier_archived: true,
                valid_until: None,
            },
            10_000,
        ),
    ];
    for (membership, price) in cases {
        let customer = CustomerLists {
            tier: Some((&gold_rule, membership)),
            ..CustomerLists::default()
        };
        let chain = PriceChain::build(Some(&customer), None, &default, today());
        assert_eq!(price_one(&chain, PCS, 1), Ok(m(price)), "{membership:?}");
    }
}

/// §12.18: price-embedded scale labels.
#[test]
fn price_embedded_labels() {
    let default = list(
        PriceListKind::Default,
        None,
        vec![entry(V, PCS, vec![fixed(1, 120_000), fixed(2, 1)])],
    );
    let branch = list(
        PriceListKind::Branch,
        None,
        vec![entry(V, PCS, vec![fixed(1, 100_000)])],
    );
    let free = list(
        PriceListKind::Default,
        None,
        vec![entry(V, PCS, vec![fixed(1, 0)])],
    );
    let none = list(PriceListKind::Default, None, vec![]);
    let kg = item(PCS);
    #[rustfmt::skip]
    let cases = [
        (None, &default, 15_000, Some(120_000), Some(125)), // 0.125 kg; the tier from 2 is ignored
        (Some(&branch), &default, 15_000, Some(100_000), Some(150)),
        (None, &free, 15_000, Some(0), None),
        (None, &none, 15_000, None, None),
    ];
    for (branch, default, amount, unit_price, qty) in cases {
        let got = price_label(branch, default, today(), &kg, m(amount)).unwrap();
        assert_eq!(
            got,
            LabelLine {
                amount: m(amount),
                unit_price: unit_price.map(m),
                qty: qty.map(Qty::from_thousandths),
            }
        );
    }
    let cheap = list(
        PriceListKind::Default,
        None,
        vec![entry(V, PCS, vec![fixed(1, 1)])],
    );
    assert_eq!(
        price_label(None, &cheap, today(), &kg, m(i64::MAX)),
        Err(PricingError::Overflow)
    );
}

#[test]
fn overflow_paths() {
    let default = default_list();
    let chain = PriceChain::store(None, &default, today());
    // The counted quantity overflows.
    let cart = [
        CartLine {
            qty: Qty::from_thousandths(i64::MAX),
            ..line(PCS, 0)
        },
        line(PCS, 1),
    ];
    assert!(
        resolve_cart(&chain, &cart)
            .into_iter()
            .all(|r| r == Err(PricingError::Overflow))
    );
    // The line amount overflows.
    let huge = CartLine {
        qty: Qty::from_thousandths(i64::MAX / 2),
        ..line(PCS, 0)
    };
    assert_eq!(
        resolve_cart(&chain, &[huge]).remove(0),
        Err(PricingError::Overflow)
    );
    // A rule overflows.
    let gold_rule = list(PriceListKind::Tier, Some(Rule::Amount(m(i64::MAX))), vec![]);
    let customer = CustomerLists {
        tier: Some((&gold_rule, gold(true))),
        ..CustomerLists::default()
    };
    let chain = PriceChain::build(Some(&customer), None, &default, today());
    assert_eq!(price_one(&chain, PCS, 1), Err(PricingError::Overflow));
}

/// §12.11 (overlap) and the list/entry validation rules.
#[test]
fn validation() {
    let invalid = |field, reason| Err(PricingError::InvalidField { field, reason });
    let rule_err = |reason| Err(PricingError::InvalidRule { reason });
    let with = |kind, rule, tiers| list(kind, rule, vec![entry(V, PCS, tiers)]);
    let eleven: Vec<_> = (1..=11).map(|i| fixed(i, 100)).collect();
    let ten: Vec<_> = (1..=10).map(|i| fixed(i, 100)).collect();
    let mut off_default = default_list();
    off_default.active = false;
    let mut archived_branch = with(PriceListKind::Branch, None, vec![fixed(1, 1)]);
    archived_branch.archived = true;
    #[rustfmt::skip]
    let cases = [
        (default_list(), Ok(())),
        (with(PriceListKind::Default, None, ten), Ok(())),
        (with(PriceListKind::Default, None, vec![fixed(1, 0)]), Ok(())),
        (with(PriceListKind::Tier, Some(Rule::Pct(-500)), vec![ruled(5, Rule::Amount(m(-100)))]), Ok(())),
        (archived_branch, Ok(())),
        (with(PriceListKind::Default, Some(Rule::Pct(-500)), vec![fixed(1, 1)]),
            rule_err("the default list cannot have a rule")),
        (with(PriceListKind::Default, None, vec![ruled(1, Rule::Pct(-500))]),
            rule_err("the default list cannot have rules")),
        (with(PriceListKind::Tier, Some(Rule::Pct(-10_000)), vec![fixed(1, 1)]),
            rule_err("a % rule must be above -100%")),
        (with(PriceListKind::Tier, None, vec![ruled(1, Rule::Pct(-12_000))]),
            rule_err("a % rule must be above -100%")),
        (off_default, invalid("active", "the default list cannot be switched off or archived")),
        (with(PriceListKind::Branch, None, vec![]), invalid("tiers", "an entry needs at least one tier")),
        (with(PriceListKind::Branch, None, eleven), invalid("tiers", "an entry has at most 10 tiers")),
        (with(PriceListKind::Branch, None, vec![fixed(0, 1)]), invalid("min_qty", "must be greater than zero")),
        (with(PriceListKind::Branch, None, vec![fixed(1, 1), fixed(2, 1), fixed(1, 2)]),
            invalid("min_qty", "must be unique within an entry")),
        (with(PriceListKind::Branch, None, vec![fixed(1, -1)]), invalid("price", "must not be negative")),
        (list(PriceListKind::Branch, None, vec![dated(PCS, Some(d(2026, 12, 2)), Some(d(2026, 12, 1)), vec![fixed(1, 1)])]),
            invalid("valid_to", "must not be before valid_from")),
    ];
    for (price_list, expected) in cases {
        assert_eq!(price_list.validate(), expected, "{price_list:?}");
    }

    // Overlap (§3): same variant + unit sharing a day.
    let nov = dated(
        PCS,
        Some(d(2026, 11, 1)),
        Some(d(2026, 11, 30)),
        vec![fixed(1, 1)],
    );
    let open = |from, to| dated(PCS, from, to, vec![fixed(1, 1)]);
    #[rustfmt::skip]
    let cases = [
        (open(Some(d(2026, 12, 1)), None), false),
        (open(None, Some(d(2026, 10, 31))), false),
        (open(Some(d(2026, 11, 30)), None), true),
        (open(None, Some(d(2026, 11, 1))), true),
        (open(None, None), true),
        (open(Some(d(2026, 11, 10)), Some(d(2026, 11, 12))), true),
        (PriceEntry { unit: BOX, ..open(None, None) }, false),
        (PriceEntry { variant: W, ..open(None, None) }, false),
    ];
    for (other, overlaps) in cases {
        assert_eq!(nov.overlaps(&other), overlaps, "{other:?}");
        assert_eq!(other.overlaps(&nov), overlaps, "{other:?}");
        let result = validate_entry(PriceListKind::Branch, &other, std::slice::from_ref(&nov));
        if overlaps {
            let err = result.unwrap_err();
            assert_eq!(err.code(), "price_overlap");
            assert_eq!(
                err,
                PricingError::PriceOverlap {
                    variant: other.variant,
                    unit: other.unit,
                    other: nov.id
                }
            );
        } else {
            assert_eq!(result, Ok(()));
        }
    }
    // Overwriting an entry is not an overlap with its stored self.
    let moved = PriceEntry {
        valid_to: Some(d(2026, 11, 29)),
        ..nov.clone()
    };
    assert_eq!(
        validate_entry(PriceListKind::Branch, &moved, std::slice::from_ref(&nov)),
        Ok(())
    );
    // A list with two overlapping entries.
    let both = list(
        PriceListKind::Branch,
        None,
        vec![nov.clone(), open(None, None)],
    );
    assert_eq!(both.validate().unwrap_err().code(), "price_overlap");
}

#[test]
fn kinds_errors_and_ids() {
    for kind in PriceListKind::ALL {
        assert_eq!(kind.to_string().parse::<PriceListKind>(), Ok(kind));
    }
    assert_eq!(
        "vip".parse::<PriceListKind>().unwrap_err().code(),
        "invalid_field"
    );

    #[rustfmt::skip]
    let cases = [
        (PricingError::InvalidField { field: "tiers", reason: "x" }, "invalid_field", "invalid tiers: x"),
        (PricingError::InvalidRule { reason: "y" }, "invalid_rule", "invalid rule: y"),
        (PricingError::NoPrice { suggestion: Some(m(1)) }, "no_price", "no price"),
        (PricingError::Overflow, "amount_overflow", "amount does not fit in 64 bits"),
    ];
    for (err, code, message) in cases {
        assert_eq!(err.code(), code);
        assert_eq!(err.to_string(), message);
    }
    let overlap = PricingError::PriceOverlap {
        variant: V,
        unit: PCS,
        other: PriceEntryId::from_uuid(Uuid::from_u128(3)),
    };
    assert!(overlap.to_string().starts_with("entry dates overlap"));

    let a = PriceListId::generate();
    let b = PriceListId::generate();
    assert!(a < b);
    assert_eq!(a.as_uuid().get_version_num(), 7);
    assert_eq!(a.to_string(), a.as_uuid().to_string());
    let e = PriceEntryId::from_uuid(Uuid::from_u128(5));
    assert_eq!(*e.as_uuid(), Uuid::from_u128(5));
    assert_eq!(e.to_string(), Uuid::from_u128(5).to_string());
}
