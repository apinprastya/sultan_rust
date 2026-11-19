//! Cash movements, their reasons, reversals, permissions and nota photos (spec 2.7 §1, §2, §2a).

use super::{CashError, invalid};
use crate::access::{Decision, Permission, PermissionSet, Requested};
use crate::ids::{CashMovementId, CashReasonId, CashierSessionId};
use crate::money::Money;

/// The longest note, in characters after trimming (§2).
pub const MAX_NOTE_CHARS: usize = 200;

/// The most nota photos per cash movement (§2a).
pub const MAX_PHOTOS: usize = 3;

/// The default company setting `nota_flag_above` (§2a): a cash out of at least this much without
/// a photo is "missing nota". `0` turns the flag off.
pub const DEFAULT_NOTA_FLAG_ABOVE: Money = Money::from_minor(100_000);

/// Which way a cash reason moves money (§1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Direction {
    /// Into the drawer.
    In,
    /// Out of the drawer.
    Out,
}

impl Direction {
    /// The stored key, `in` or `out`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::In => "in",
            Self::Out => "out",
        }
    }

    /// The direction with this key.
    #[must_use]
    pub fn from_key(key: &str) -> Option<Self> {
        [Self::In, Self::Out]
            .into_iter()
            .find(|d| d.as_str() == key)
    }
}

/// The kind of a cash movement (§1, K2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum CashKind {
    /// Cash into the drawer, with an `in` reason.
    In,
    /// Cash out of the drawer (an expense), with an `out` reason.
    Out,
    /// A safe drop: cash from the drawer to the safe, without a reason.
    Drop,
}

impl CashKind {
    /// Every kind, in report order.
    pub const ALL: [Self; 3] = [Self::In, Self::Out, Self::Drop];

    /// The stored key: `in`, `out`, `drop`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::In => "in",
            Self::Out => "out",
            Self::Drop => "drop",
        }
    }

    /// The kind with this key.
    #[must_use]
    pub fn from_key(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.as_str() == key)
    }

    /// The reason direction this kind needs; `None` for a drop, which has no reason.
    #[must_use]
    pub const fn direction(self) -> Option<Direction> {
        match self {
            Self::In => Some(Direction::In),
            Self::Out => Some(Direction::Out),
            Self::Drop => None,
        }
    }

    /// The permission it needs (§2, §7): `cash.in`, `cash.out` or `cash.drop`.
    #[must_use]
    pub const fn permission(self) -> Permission {
        match self {
            Self::In => Permission::CashIn,
            Self::Out => Permission::CashOut,
            Self::Drop => Permission::CashDrop,
        }
    }

    /// The value checked against the permission's limit: the amount for `cash.out` (`MaxAmount` per
    /// movement, K4), nothing for the others.
    #[must_use]
    pub const fn requested(self, amount: Money) -> Option<Requested> {
        match self {
            Self::Out => Some(Requested::Amount(amount)),
            Self::In | Self::Drop => None,
        }
    }
}

/// A cash in / out reason (HQ master data, §1, K1).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CashReason {
    /// The ID.
    pub id: CashReasonId,
    /// The name shown and printed.
    pub name: String,
    /// In or out.
    pub direction: Direction,
    /// `false` once archived: refused for new movements, still shown on old ones.
    pub active: bool,
    /// The list order.
    pub sort: i32,
    /// A note is required (the seeded "Other").
    pub note_required: bool,
}

/// A seeded cash reason (§2), in Indonesian (the stored name) and English.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SeedReason {
    /// The Indonesian name, stored as the reason's name.
    pub name_id: &'static str,
    /// The English name.
    pub name_en: &'static str,
    /// In or out.
    pub direction: Direction,
    /// The list order within its direction.
    pub sort: i32,
    /// A note is required.
    pub note_required: bool,
}

/// The seeded cash reasons (§2), editable at HQ.
pub const SEED_REASONS: [SeedReason; 6] = [
    SeedReason {
        name_id: "Tambahan modal / uang kecil",
        name_en: "Float top-up from the safe",
        direction: Direction::In,
        sort: 10,
        note_required: false,
    },
    SeedReason {
        name_id: "Lainnya",
        name_en: "Other",
        direction: Direction::In,
        sort: 90,
        note_required: true,
    },
    SeedReason {
        name_id: "Belanja keperluan toko",
        name_en: "Shop supplies",
        direction: Direction::Out,
        sort: 10,
        note_required: false,
    },
    SeedReason {
        name_id: "Ongkos kirim / parkir",
        name_en: "Delivery, parking",
        direction: Direction::Out,
        sort: 20,
        note_required: false,
    },
    SeedReason {
        name_id: "Kebersihan / keamanan",
        name_en: "Cleaning, security",
        direction: Direction::Out,
        sort: 30,
        note_required: false,
    },
    SeedReason {
        name_id: "Lainnya",
        name_en: "Other",
        direction: Direction::Out,
        sort: 90,
        note_required: true,
    },
];

/// A posted cash movement: what the rules need of it (§1). Created posted, never edited (no
/// draft); the number, branch, register, user, approver and time are stored next to it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CashMovement {
    /// The ID (client `UUIDv7`).
    pub id: CashMovementId,
    /// The cashier session it belongs to.
    pub session: CashierSessionId,
    /// In, out or drop.
    pub kind: CashKind,
    /// The amount, above 0.
    pub amount: Money,
    /// The reason (in and out); `None` for a drop.
    pub reason: Option<CashReasonId>,
    /// The trimmed note.
    pub note: Option<String>,
    /// The movement this one reverses (K10).
    pub reverses: Option<CashMovementId>,
}

impl CashMovement {
    /// Validates a new cash in / out / drop of the open `session` (§2): amount above 0; in and out
    /// need an active `reason` of the matching direction, a drop takes none; the note is trimmed,
    /// at most [`MAX_NOTE_CHARS`] characters, and required when the reason says so.
    ///
    /// # Errors
    /// [`CashError::InvalidField`] with field `amount`, `reason` or `note`.
    pub fn new(
        id: CashMovementId,
        session: CashierSessionId,
        kind: CashKind,
        amount: Money,
        reason: Option<&CashReason>,
        note: Option<&str>,
    ) -> Result<Self, CashError> {
        if amount <= Money::ZERO {
            return Err(invalid("amount", "must be above 0"));
        }
        let reason = match (kind.direction(), reason) {
            (None, None) => None,
            (None, Some(_)) => return Err(invalid("reason", "a safe drop has no reason")),
            (Some(_), None) => return Err(invalid("reason", "required")),
            (Some(direction), Some(r)) => {
                if r.direction != direction {
                    return Err(invalid("reason", "the reason is for the other direction"));
                }
                if !r.active {
                    return Err(invalid("reason", "the reason is archived"));
                }
                Some(r)
            }
        };
        let note = clean_note(note, reason.is_some_and(|r| r.note_required))?;
        Ok(Self {
            id,
            session,
            kind,
            amount,
            reason: reason.map(|r| r.id),
            note,
            reverses: None,
        })
    }

    /// The reversal of `target` (K10): `target` must belong to `open_session` (the caller's open
    /// session), must not be a reversal itself and must not be reversed yet (`already_reversed`).
    /// The reversal takes the original's kind, amount and reason, has the opposite effect, and
    /// needs a note. The original's reason may be archived since: a reversal is still allowed.
    ///
    /// # Errors
    /// [`CashError::InvalidField`] (`reverses` or `note`) or [`CashError::AlreadyReversed`].
    pub fn reversal(
        id: CashMovementId,
        open_session: CashierSessionId,
        target: &Self,
        already_reversed: bool,
        note: Option<&str>,
    ) -> Result<Self, CashError> {
        if target.session != open_session {
            return Err(invalid("reverses", "not a movement of this open session"));
        }
        if target.is_reversal() {
            return Err(invalid("reverses", "a reversal cannot be reversed"));
        }
        if already_reversed {
            return Err(CashError::AlreadyReversed);
        }
        let note = clean_note(note, true)?;
        Ok(Self {
            id,
            session: open_session,
            kind: target.kind,
            amount: target.amount,
            reason: target.reason,
            note,
            reverses: Some(target.id),
        })
    }

    /// Whether this movement reverses another.
    #[must_use]
    pub const fn is_reversal(&self) -> bool {
        self.reverses.is_some()
    }

    /// The effect on expected cash (§2, §4): `in` adds, `out` and `drop` subtract; a reversal
    /// the opposite.
    #[must_use]
    pub const fn effect(&self) -> Money {
        let adds = matches!(self.kind, CashKind::In) != self.is_reversal();
        if adds {
            self.amount
        } else {
            // `amount` is above 0, so its negation fits.
            Money::from_minor(-self.amount.minor())
        }
    }

    /// The permission check of this movement (§2, §7): `cash.in` and `cash.drop` without a
    /// limit, `cash.out` against its `MaxAmount` per movement. A reversal needs the original's
    /// permission. A permission the user lacks gives [`Decision::NeedsOverride`] (all three are
    /// overridable at the till; the override's value is the amount).
    #[must_use]
    pub fn check(&self, permissions: &PermissionSet) -> Decision {
        permissions.check(self.kind.permission(), self.kind.requested(self.amount))
    }
}

/// Trims a note (§2): blank is `None`; more than [`MAX_NOTE_CHARS`] characters, or none while
/// `required`, is refused.
///
/// # Errors
/// [`CashError::InvalidField`] with field `note`.
pub fn clean_note(note: Option<&str>, required: bool) -> Result<Option<String>, CashError> {
    let note = note.map(str::trim).filter(|n| !n.is_empty());
    match note {
        None if required => Err(invalid("note", "required")),
        Some(n) if n.chars().count() > MAX_NOTE_CHARS => {
            Err(invalid("note", "at most 200 characters"))
        }
        n => Ok(n.map(str::to_owned)),
    }
}

/// Whether `id` is reversed by one of `movements`.
#[must_use]
pub fn is_reversed(id: CashMovementId, movements: &[CashMovement]) -> bool {
    movements.iter().any(|m| m.reverses == Some(id))
}

/// Whether `movement` is "missing nota" (§2a): a cash out (not a reversal) with an amount of at
/// least `flag_above` (`0` = off), no photo, and not reversed. Never blocks posting.
#[must_use]
pub fn is_missing_nota(
    movement: &CashMovement,
    photos: usize,
    reversed: bool,
    flag_above: Money,
) -> bool {
    movement.kind == CashKind::Out
        && !movement.is_reversal()
        && flag_above > Money::ZERO
        && movement.amount >= flag_above
        && photos == 0
        && !reversed
}

/// Checks that one more photo fits on a movement that has `photos` already (§2a).
///
/// # Errors
/// [`CashError::PhotoLimit`] at [`MAX_PHOTOS`].
pub const fn ensure_photo_slot(photos: usize) -> Result<(), CashError> {
    if photos < MAX_PHOTOS {
        Ok(())
    } else {
        Err(CashError::PhotoLimit { max: MAX_PHOTOS })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::access::{Grant, Limit};

    fn m(v: i64) -> Money {
        Money::from_minor(v)
    }

    fn reason(direction: Direction, active: bool, note_required: bool) -> CashReason {
        CashReason {
            id: CashReasonId::generate(),
            name: "Lainnya".into(),
            direction,
            active,
            sort: 90,
            note_required,
        }
    }

    /// Name, kind, amount, reason, note → the stored note, or the field of the error.
    type NewCase<'a> = (
        &'a str,
        CashKind,
        i64,
        Option<&'a CashReason>,
        Option<&'a str>,
        Result<Option<&'a str>, &'a str>,
    );

    /// Name, open session, target, already reversed, note → the error.
    type ReversalCase<'a> = (
        &'a str,
        CashierSessionId,
        &'a CashMovement,
        bool,
        Option<&'a str>,
        CashError,
    );

    fn movement(kind: CashKind, amount: i64) -> CashMovement {
        CashMovement {
            id: CashMovementId::generate(),
            session: CashierSessionId::generate(),
            kind,
            amount: m(amount),
            reason: None,
            note: None,
            reverses: None,
        }
    }

    #[test]
    fn keys_round_trip() {
        for k in CashKind::ALL {
            assert_eq!(CashKind::from_key(k.as_str()), Some(k));
        }
        for d in [Direction::In, Direction::Out] {
            assert_eq!(Direction::from_key(d.as_str()), Some(d));
        }
        assert_eq!(CashKind::from_key("pickup"), None);
        assert_eq!(Direction::from_key("IN"), None);
        assert_eq!(
            CashKind::ALL.map(CashKind::permission),
            [
                Permission::CashIn,
                Permission::CashOut,
                Permission::CashDrop
            ]
        );
    }

    /// Spec §12 cases 5, 6 and 7.
    #[test]
    fn new_movement_rules() {
        let shop = reason(Direction::Out, true, false);
        let other_out = reason(Direction::Out, true, true);
        let top_up = reason(Direction::In, true, false);
        let archived = reason(Direction::Out, false, false);
        let long = "x".repeat(201);
        let max = "é".repeat(200);
        #[rustfmt::skip]
        let cases: [NewCase<'_>; 16] = [
            ("cash out", CashKind::Out, 25_000, Some(&shop), None, Ok(None)),
            ("cash in", CashKind::In, 50_000, Some(&top_up), Some("  dari brankas "), Ok(Some("dari brankas"))),
            ("drop", CashKind::Drop, 300_000, None, None, Ok(None)),
            ("drop with a note", CashKind::Drop, 1, None, Some("pickup"), Ok(Some("pickup"))),
            ("other with a note", CashKind::Out, 5_000, Some(&other_out), Some("galon"), Ok(Some("galon"))),
            ("200 characters", CashKind::Out, 5_000, Some(&shop), Some(&max), Ok(Some(&max))),
            ("amount 0", CashKind::Out, 0, Some(&shop), None, Err("amount")),
            ("negative", CashKind::In, -1, Some(&top_up), None, Err("amount")),
            ("out reason on in", CashKind::In, 1_000, Some(&shop), None, Err("reason")),
            ("in reason on out", CashKind::Out, 1_000, Some(&top_up), None, Err("reason")),
            ("drop with a reason", CashKind::Drop, 1_000, Some(&shop), None, Err("reason")),
            ("in without a reason", CashKind::In, 1_000, None, None, Err("reason")),
            ("archived reason", CashKind::Out, 1_000, Some(&archived), None, Err("reason")),
            ("other without a note", CashKind::Out, 1_000, Some(&other_out), None, Err("note")),
            ("other with a blank note", CashKind::Out, 1_000, Some(&other_out), Some("   "), Err("note")),
            ("201 characters", CashKind::Out, 1_000, Some(&shop), Some(&long), Err("note")),
        ];
        let session = CashierSessionId::generate();
        for (name, kind, amount, reason, note, expected) in cases {
            let id = CashMovementId::generate();
            let got = CashMovement::new(id, session, kind, m(amount), reason, note);
            match expected {
                Ok(note) => {
                    let got = got.unwrap();
                    assert_eq!(got.note.as_deref(), note, "{name}");
                    assert_eq!(got.reason, reason.map(|r| r.id), "{name}");
                    assert_eq!((got.id, got.session, got.kind), (id, session, kind));
                    assert!(!got.is_reversal(), "{name}");
                }
                Err(field) => {
                    let err = got.unwrap_err();
                    assert_eq!(err.code(), "invalid_field", "{name}");
                    assert_eq!(err.field(), Some(field), "{name}");
                }
            }
        }
    }

    /// Spec §12 case 8 (the rules; the expected cash is in `figures`).
    #[test]
    fn reversal_rules() {
        let session = CashierSessionId::generate();
        let shop = reason(Direction::Out, true, false);
        let out = CashMovement::new(
            CashMovementId::generate(),
            session,
            CashKind::Out,
            m(25_000),
            Some(&shop),
            None,
        )
        .unwrap();
        let rev = CashMovement::reversal(
            CashMovementId::generate(),
            session,
            &out,
            false,
            Some(" typo "),
        )
        .unwrap();
        assert_eq!(
            (
                rev.kind,
                rev.amount,
                rev.reason,
                rev.reverses,
                rev.note.as_deref()
            ),
            (out.kind, out.amount, out.reason, Some(out.id), Some("typo"))
        );
        assert_eq!((out.effect(), rev.effect()), (m(-25_000), m(25_000)));
        assert!(is_reversed(out.id, &[out.clone(), rev.clone()]));
        assert!(!is_reversed(rev.id, &[out.clone(), rev.clone()]));

        let other = CashierSessionId::generate();
        let id = CashMovementId::generate();
        #[rustfmt::skip]
        let cases: [ReversalCase<'_>; 5] = [
            ("again", session, &out, true, Some("x"), CashError::AlreadyReversed),
            ("a reversal", session, &rev, false, Some("x"), invalid("reverses", "a reversal cannot be reversed")),
            ("other session", other, &out, false, Some("x"), invalid("reverses", "not a movement of this open session")),
            ("no note", session, &out, false, None, invalid("note", "required")),
            ("blank note", session, &out, false, Some(" "), invalid("note", "required")),
        ];
        for (name, open, target, reversed, note, err) in cases {
            assert_eq!(
                CashMovement::reversal(id, open, target, reversed, note),
                Err(err),
                "{name}"
            );
        }
        // Effects of every kind and their reversals.
        for (kind, effect) in [
            (CashKind::In, 10),
            (CashKind::Out, -10),
            (CashKind::Drop, -10),
        ] {
            let mut mv = movement(kind, 10);
            assert_eq!(mv.effect(), m(effect));
            mv.reverses = Some(CashMovementId::generate());
            assert_eq!(mv.effect(), m(-effect));
        }
    }

    /// Spec §12 cases 3 and 4.
    #[test]
    fn permission_checks() {
        let cashier: PermissionSet = [
            Grant::unlimited(Permission::CashIn),
            Grant::limited(Permission::CashOut, Limit::MaxAmount(m(50_000))).unwrap(),
        ]
        .into_iter()
        .collect();
        let manager: PermissionSet = [
            Grant::unlimited(Permission::CashIn),
            Grant::limited(Permission::CashOut, Limit::MaxAmount(m(2_000_000))).unwrap(),
            Grant::unlimited(Permission::CashDrop),
        ]
        .into_iter()
        .collect();
        let none = PermissionSet::default();
        let cashier_limit = Some(Limit::MaxAmount(m(50_000)));
        #[rustfmt::skip]
        let cases = [
            ("out at the limit", &cashier, CashKind::Out, 50_000, Decision::Allowed),
            ("out above the limit", &cashier, CashKind::Out, 50_001, Decision::NeedsOverride { limit: cashier_limit }),
            ("manager approves", &manager, CashKind::Out, 50_001, Decision::Allowed),
            ("manager up to 2 000 000", &manager, CashKind::Out, 2_000_000, Decision::Allowed),
            ("in, no limit", &cashier, CashKind::In, 9_000_000_000, Decision::Allowed),
            ("drop by a cashier", &cashier, CashKind::Drop, 100, Decision::NeedsOverride { limit: None }),
            ("drop by a manager, no limit", &manager, CashKind::Drop, 9_000_000_000, Decision::Allowed),
            ("in without anything", &none, CashKind::In, 1, Decision::NeedsOverride { limit: None }),
        ];
        for (name, perms, kind, amount, expected) in cases {
            let mut mv = movement(kind, amount);
            assert_eq!(mv.check(perms), expected, "{name}");
            // A reversal needs the original's permission.
            mv.reverses = Some(CashMovementId::generate());
            assert_eq!(mv.check(perms), expected, "{name} (reversal)");
        }
    }

    /// Spec §12 case 33 (the pure rule) and §2a's photo limit.
    #[test]
    fn missing_nota_and_photo_limit() {
        let flag = DEFAULT_NOTA_FLAG_ABOVE;
        let mut reversal = movement(CashKind::Out, 150_000);
        reversal.reverses = Some(CashMovementId::generate());
        #[rustfmt::skip]
        let cases = [
            ("99 999", movement(CashKind::Out, 99_999), 0, false, flag, false),
            ("100 000", movement(CashKind::Out, 100_000), 0, false, flag, true),
            ("150 000 with a photo", movement(CashKind::Out, 150_000), 1, false, flag, false),
            ("reversed", movement(CashKind::Out, 150_000), 0, true, flag, false),
            ("the reversal itself", reversal, 0, false, flag, false),
            ("setting 0 = off", movement(CashKind::Out, 150_000), 0, false, m(0), false),
            ("cash in", movement(CashKind::In, 150_000), 0, false, flag, false),
            ("drop", movement(CashKind::Drop, 150_000), 0, false, flag, false),
        ];
        for (name, mv, photos, reversed, flag_above, expected) in cases {
            assert_eq!(
                is_missing_nota(&mv, photos, reversed, flag_above),
                expected,
                "{name}"
            );
        }
        assert_eq!(MAX_PHOTOS, 3);
        for (photos, ok) in [(0, true), (2, true), (3, false), (4, false)] {
            assert_eq!(ensure_photo_slot(photos).is_ok(), ok, "{photos}");
        }
        assert_eq!(ensure_photo_slot(3).unwrap_err().code(), "photo_limit");
    }

    #[test]
    fn seeded_reasons() {
        let names: Vec<_> = SEED_REASONS
            .iter()
            .map(|r| (r.direction.as_str(), r.name_id, r.note_required))
            .collect();
        assert_eq!(
            names,
            [
                ("in", "Tambahan modal / uang kecil", false),
                ("in", "Lainnya", true),
                ("out", "Belanja keperluan toko", false),
                ("out", "Ongkos kirim / parkir", false),
                ("out", "Kebersihan / keamanan", false),
                ("out", "Lainnya", true),
            ]
        );
        assert!(SEED_REASONS.iter().all(|r| !r.name_en.is_empty()));
    }
}
