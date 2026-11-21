//! Cash reasons, cash movements, nota photos, the session inputs of the close report and the
//! float between sessions (spec 2.7, phase K2).

use std::collections::BTreeMap;

use sultan_core::cash::{
    CashError, CashKind, CashMovement, CashReason, Direction, ReasonLine, SEED_REASONS,
    SessionInput, session_figures,
};
use sultan_core::ids::{
    CashMovementId, CashReasonId, CashierSessionId, NotaPhotoId, RegisterId, UserId,
};
use sultan_core::money::Money;
use sultan_core::sale::SessionEvent;
use sultan_db::audit::{self, AuditEntry};
use sultan_db::cash::{
    self, CashDbError, CashFilter, CashMovementRecord, CashResult, NewCashMovement, NotaPhoto,
    SEED_REASON_IDS, Written,
};
use sultan_db::org::{Branch, Register};
use sultan_db::{Db, cashier_sessions};

use crate::catalog_fixtures::{actor, trail};
use crate::common::{count, db_tests, raw, t};
use crate::sales_fixtures::{NOV16, Till, hash_of, open_session, rp, till};

db_tests!(
    cash_reasons_seed_put_and_archive,
    numbers_follow_the_local_day,
    replay_and_conflicting_body,
    reversal_once,
    reason_and_session_checks,
    lists_filter_and_page,
    photos_limit_and_replay,
    movements_are_append_only,
    session_inputs_feed_the_figures,
    float_between_sessions,
    close_keeps_the_reasons,
);

/// An hour in milliseconds.
const HOUR: i64 = 3_600_000;

/// The seeded reasons by position: …0401 float top-up (in), …0402 other (in), …0403 shop
/// supplies (out), …0404 delivery / parking (out), …0405 cleaning (out), …0406 other (out).
const TOP_UP: usize = 0;
const OTHER_OUT: usize = 5;
const PARKING: usize = 3;

async fn reason(db: &Db, id: CashReasonId) -> CashReason {
    cash::get_cash_reason(db, id)
        .await
        .unwrap()
        .unwrap()
        .for_rules()
}

/// A movement of the till's open session at `at`.
fn new_mv(till: &Till, movement: CashMovement, at: i64, salt: u8) -> NewCashMovement {
    NewCashMovement {
        request_hash: hash_of(*movement.id.as_uuid(), salt),
        movement,
        branch: till.o.jkt.id,
        register: till.o.register.id,
        user: till.o.cashier.id,
        approved_by: None,
        posted_at: t(at),
    }
}

async fn post(db: &Db, new: &NewCashMovement) -> CashResult<(Written, CashMovementRecord)> {
    let mut tx = db.begin().await.unwrap();
    let out = cash::post_cash_movement(&mut tx, new, t(at_of(new))).await;
    if out.is_ok() {
        tx.commit().await.unwrap();
    }
    out
}

fn at_of(new: &NewCashMovement) -> i64 {
    new.posted_at.unix_millis()
}

/// Posts a valid movement of `kind` on the till's session.
async fn mv(
    db: &Db,
    till: &Till,
    kind: CashKind,
    amount: i64,
    reason_at: Option<usize>,
    at: i64,
) -> CashMovementRecord {
    let r = match reason_at {
        Some(i) => Some(reason(db, SEED_REASON_IDS[i]).await),
        None => None,
    };
    let m = CashMovement::new(
        CashMovementId::generate(),
        till.session,
        kind,
        rp(amount),
        r.as_ref(),
        Some("nota"),
    )
    .unwrap();
    post(db, &new_mv(till, m, at, 1)).await.unwrap().1
}

/// Reverses `original` on the till's session.
async fn reverse(db: &Db, till: &Till, original: &CashMovementRecord, at: i64) -> CashResult<()> {
    let m = CashMovement::reversal(
        CashMovementId::generate(),
        till.session,
        &original.movement,
        false,
        Some("salah input"),
    )
    .unwrap();
    post(db, &new_mv(till, m, at, 2)).await.map(|_| ())
}

#[allow(clippy::too_many_lines)] // one story per area, like the sales tests
async fn cash_reasons_seed_put_and_archive(db: &Db) {
    let till = till(db).await;
    let active = cash::list_cash_reasons(db, false).await.unwrap();
    assert_eq!(active.len(), SEED_REASONS.len());
    // In before out, then by sort: the seed order.
    for ((stored, seed), id) in active.iter().zip(SEED_REASONS).zip(SEED_REASON_IDS) {
        assert_eq!(stored.reason.id, id);
        assert_eq!(stored.reason.name, seed.name_id);
        assert_eq!(stored.reason.direction, seed.direction);
        assert_eq!(stored.reason.sort, seed.sort);
        assert_eq!(stored.reason.note_required, seed.note_required);
        assert!(stored.reason.active && stored.archived_at.is_none());
    }
    let who = actor(&till.o);
    let put = |r: CashReason| {
        let who = &who;
        async move {
            let mut tx = db.begin().await.unwrap();
            let out = cash::put_cash_reason(&mut tx, &r, who, t(5)).await;
            tx.commit().await.unwrap();
            out
        }
    };
    let before = trail(db).await;
    let mut gallon = CashReason {
        id: CashReasonId::generate(),
        name: "  Air galon ".into(),
        direction: Direction::Out,
        active: true,
        sort: 40,
        note_required: false,
    };
    assert_eq!(put(gallon.clone()).await.unwrap(), Written::Created);
    assert_eq!(trail(db).await, (before.0 + 1, before.1 + 1));
    let stored = cash::get_cash_reason(db, gallon.id).await.unwrap().unwrap();
    assert_eq!(stored.reason.name, "Air galon");
    // A replay (the name trims to the same) writes nothing; a change updates.
    assert_eq!(put(gallon.clone()).await.unwrap(), Written::Unchanged);
    assert_eq!(trail(db).await, (before.0 + 1, before.1 + 1));
    gallon.sort = -5;
    gallon.note_required = true;
    assert_eq!(put(gallon.clone()).await.unwrap(), Written::Updated);
    let stored = cash::get_cash_reason(db, gallon.id).await.unwrap().unwrap();
    assert_eq!(
        (stored.reason.sort, stored.reason.note_required),
        (-5, true)
    );
    // Out reasons list after in reasons, by sort.
    let active = cash::list_cash_reasons(db, false).await.unwrap();
    assert_eq!(active[2].reason.id, gallon.id);
    // Refusals.
    for (bad, field) in [
        (
            CashReason {
                direction: Direction::In,
                ..gallon.clone()
            },
            "direction",
        ),
        (
            CashReason {
                name: "   ".into(),
                ..gallon.clone()
            },
            "name",
        ),
        (
            CashReason {
                name: "x".repeat(cash::MAX_REASON_NAME + 1),
                ..gallon.clone()
            },
            "name",
        ),
    ] {
        match put(bad).await {
            Err(CashDbError::Invalid { field: f, .. }) => assert_eq!(f, field),
            other => panic!("{other:?}"),
        }
    }
    // Archive: hidden from registers, listed with archived ones, refused by the rules.
    let archive = |id| {
        let who = &who;
        async move {
            let mut tx = db.begin().await.unwrap();
            let out = cash::archive_cash_reason(&mut tx, id, who, t(6)).await;
            tx.commit().await.unwrap();
            out
        }
    };
    assert_eq!(archive(gallon.id).await.unwrap(), Written::Updated);
    assert_eq!(archive(gallon.id).await.unwrap(), Written::Unchanged);
    assert!(matches!(
        archive(CashReasonId::generate()).await,
        Err(CashDbError::NotFound("cash reason"))
    ));
    assert!(matches!(
        put(gallon.clone()).await,
        Err(CashDbError::Archived)
    ));
    assert!(
        !cash::list_cash_reasons(db, false)
            .await
            .unwrap()
            .iter()
            .any(|r| r.reason.id == gallon.id)
    );
    let all = cash::list_cash_reasons(db, true).await.unwrap();
    let archived = all.iter().find(|r| r.reason.id == gallon.id).unwrap();
    assert_eq!(archived.archived_at, Some(t(6)));
    assert!(archived.reason.active);
    assert!(!archived.for_rules().active);
    // An inactive (not archived) reason is hidden from registers too.
    let mut quiet = reason(db, SEED_REASON_IDS[PARKING]).await;
    quiet.active = false;
    assert_eq!(put(quiet).await.unwrap(), Written::Updated);
    assert_eq!(cash::list_cash_reasons(db, false).await.unwrap().len(), 5);
    // The database refuses an unknown direction.
    assert!(
        raw(
            db,
            "UPDATE cash_reason SET direction = 'sideways' WHERE sort_order = 10"
        )
        .await
        .is_err()
    );
}

/// Spec 2.7 §12 case 14 (the database half): numbers per register and branch-local day, across
/// midnight in `Asia/Jakarta` and in `Asia/Jayapura`.
async fn numbers_follow_the_local_day(db: &Db) {
    let till = till(db).await;
    // 2026-11-16 23:59:59.999 WIB, then 00:00 WIB on the 17th.
    let midnight = NOV16 + 14 * HOUR;
    let first = mv(db, &till, CashKind::Drop, 100_000, None, midnight - 1).await;
    let second = mv(db, &till, CashKind::In, 50_000, Some(TOP_UP), midnight - 1).await;
    let next_day = mv(db, &till, CashKind::Drop, 100_000, None, midnight).await;
    assert_eq!(first.number, "JKT01-R1-K-261116-01");
    assert_eq!(second.number, "JKT01-R1-K-261116-02");
    assert_eq!(next_day.number, "JKT01-R1-K-261117-01");
    let by_number = cash::get_cash_movement_by_number(db, &second.number)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(by_number, second);

    // A Jayapura branch (WIT, +9) rolls over two hours earlier.
    let papua = Branch {
        timezone: "Asia/Jayapura".into(),
        ..till.o.bdg.clone()
    };
    let papua_till = Register {
        id: RegisterId::generate(),
        branch: papua.id,
        code: "R1".into(),
        name: "Kasir".into(),
        default_location: None,
        archived: false,
    };
    let mut tx = db.begin().await.unwrap();
    sultan_db::org::save_branch(&mut tx, &papua, t(2))
        .await
        .unwrap();
    sultan_db::org::save_register(&mut tx, &papua_till, t(2))
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let session = open_session(db, till.o.manager.id, papua_till.id, NOV16).await;
    let wit_midnight = NOV16 + 12 * HOUR;
    for (at, number) in [
        (wit_midnight - 1, "BDG01-R1-K-261116-01"),
        (wit_midnight, "BDG01-R1-K-261117-01"),
        (wit_midnight + 1, "BDG01-R1-K-261117-02"),
    ] {
        let m = CashMovement::new(
            CashMovementId::generate(),
            session,
            CashKind::Drop,
            rp(1),
            None,
            None,
        )
        .unwrap();
        let new = NewCashMovement {
            branch: papua.id,
            register: papua_till.id,
            user: till.o.manager.id,
            ..new_mv(&till, m, at, 1)
        };
        assert_eq!(post(db, &new).await.unwrap().1.number, number);
    }
    // The register belongs to another branch than the one named.
    let m = CashMovement::new(
        CashMovementId::generate(),
        session,
        CashKind::Drop,
        rp(1),
        None,
        None,
    )
    .unwrap();
    let wrong = NewCashMovement {
        register: papua_till.id,
        user: till.o.manager.id,
        ..new_mv(&till, m, wit_midnight, 1)
    };
    match post(db, &wrong).await {
        Err(CashDbError::Invalid { field, .. }) => assert_eq!(field, "register"),
        other => panic!("{other:?}"),
    }
}

/// Spec 2.7 §12 case 16 (the database half): the same body again returns the stored movement,
/// another body is a conflict.
async fn replay_and_conflicting_body(db: &Db) {
    let till = till(db).await;
    let parking = reason(db, SEED_REASON_IDS[PARKING]).await;
    let m = CashMovement::new(
        CashMovementId::generate(),
        till.session,
        CashKind::Out,
        rp(15_000),
        Some(&parking),
        Some("  parkir  "),
    )
    .unwrap();
    let new = NewCashMovement {
        approved_by: Some(till.o.manager.id),
        ..new_mv(&till, m.clone(), NOV16, 1)
    };
    let before = trail(db).await;
    let (written, first) = post(db, &new).await.unwrap();
    assert_eq!(written, Written::Created);
    assert_eq!(trail(db).await, (before.0 + 1, before.1 + 1));
    assert_eq!(first.movement, m);
    assert_eq!(first.movement.note.as_deref(), Some("parkir"));
    assert_eq!(first.approved_by, Some(till.o.manager.id));
    assert_eq!(first.reason_name.as_deref(), Some("Ongkos kirim / parkir"));
    assert_eq!((first.photos, first.reversed_by), (0, None));
    // Replay: the stored movement, nothing written, even an hour later.
    let later = NewCashMovement {
        posted_at: t(NOV16 + HOUR),
        ..new.clone()
    };
    let (written, again) = post(db, &later).await.unwrap();
    assert_eq!((written, &again), (Written::Unchanged, &first));
    assert_eq!(trail(db).await, (before.0 + 1, before.1 + 1));
    let mut tx = db.begin().await.unwrap();
    assert_eq!(
        cash::replayed_cash_movement(&mut tx, m.id, &new.request_hash)
            .await
            .unwrap(),
        Some(first.clone())
    );
    assert_eq!(
        cash::replayed_cash_movement(&mut tx, CashMovementId::generate(), &new.request_hash)
            .await
            .unwrap(),
        None
    );
    drop(tx);
    // Another body.
    let other = NewCashMovement {
        request_hash: hash_of(*m.id.as_uuid(), 9),
        ..new.clone()
    };
    match post(db, &other).await {
        Err(e @ CashDbError::Conflict("cash movement", id)) => {
            assert_eq!(id, *m.id.as_uuid());
            assert_eq!(e.code(), Some("conflict"));
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(count(db, "cash_movement").await, 1);
    // The audit row names the approver; the outbox carries the movement.
    let entries = audit::query(
        db,
        &audit::AuditFilter {
            action: Some("cash_movement.post".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(entries.len(), 1);
    let after = entries[0].after.as_ref().unwrap();
    assert_eq!(after["number"], "JKT01-R1-K-261116-01");
    assert_eq!(after["approved_by"], till.o.manager.id.to_string());
    assert_eq!(entries[0].register, Some(till.o.register.id));
}

/// Spec 2.7 §12 case 8 (the database half): a movement is reversed at most once.
#[allow(clippy::too_many_lines)] // one story per area, like the sales tests
async fn reversal_once(db: &Db) {
    let till = till(db).await;
    let out = mv(db, &till, CashKind::Out, 25_000, Some(PARKING), NOV16).await;
    reverse(db, &till, &out, NOV16 + 1).await.unwrap();
    let stored = cash::get_cash_movement(db, out.movement.id)
        .await
        .unwrap()
        .unwrap();
    let reversal_id = stored.reversed_by.unwrap();
    let reversal = cash::get_cash_movement(db, reversal_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        reversal.reverses_number.as_deref(),
        Some(out.number.as_str())
    );
    assert_eq!(reversal.movement.reverses, Some(out.movement.id));
    assert_eq!(reversal.movement.kind, CashKind::Out);
    // Netted: the cash out and its reversal cancel out in the session events.
    assert!(
        cash::session_cash_events(db, till.session)
            .await
            .unwrap()
            .is_empty()
    );
    // Again → already_reversed.
    match reverse(db, &till, &out, NOV16 + 2).await {
        Err(CashDbError::Rule(CashError::AlreadyReversed)) => {}
        other => panic!("{other:?}"),
    }
    // A reversal of a reversal, of an unknown movement, of another session, or one that does
    // not copy its original → invalid_field `reverses`.
    let reverse_raw = |m: CashMovement| {
        let till = &till;
        async move { post(db, &new_mv(till, m, NOV16 + 3, 3)).await.map(|_| ()) }
    };
    let base = CashMovement {
        id: CashMovementId::generate(),
        note: Some("x".into()),
        reverses: Some(reversal_id),
        ..reversal.movement.clone()
    };
    let r2 = register_two(db, &till).await;
    let other_session = open_session(db, till.o.manager.id, r2, NOV16).await;
    let foreign = {
        let m = CashMovement::new(
            CashMovementId::generate(),
            other_session,
            CashKind::Drop,
            rp(5),
            None,
            None,
        )
        .unwrap();
        let new = NewCashMovement {
            register: r2,
            user: till.o.manager.id,
            ..new_mv(&till, m, NOV16, 1)
        };
        post(db, &new).await.unwrap().1
    };
    for m in [
        base.clone(),
        CashMovement {
            reverses: Some(CashMovementId::generate()),
            ..base.clone()
        },
        CashMovement {
            reverses: Some(foreign.movement.id),
            kind: CashKind::Drop,
            reason: None,
            amount: rp(5),
            ..base.clone()
        },
    ] {
        match reverse_raw(m).await {
            Err(CashDbError::Invalid { field, .. }) => assert_eq!(field, "reverses"),
            other => panic!("{other:?}"),
        }
    }
    let safe_drop = mv(db, &till, CashKind::Drop, 70_000, None, NOV16 + 4).await;
    let copy = CashMovement {
        id: CashMovementId::generate(),
        reverses: Some(safe_drop.movement.id),
        note: Some("x".into()),
        ..safe_drop.movement.clone()
    };
    for m in [
        CashMovement {
            amount: rp(1),
            ..copy.clone()
        },
        CashMovement {
            kind: CashKind::In,
            reason: Some(SEED_REASON_IDS[TOP_UP]),
            ..copy.clone()
        },
    ] {
        match reverse_raw(m).await {
            Err(CashDbError::Invalid { field, .. }) => assert_eq!(field, "reverses"),
            other => panic!("{other:?}"),
        }
    }
    reverse_raw(copy.clone()).await.unwrap();
    // The unique key is the last guard: a second reversal row cannot be stored.
    assert!(
        raw(
            db,
            "INSERT INTO cash_movement (id, number, kind, amount, reason_id, note, branch_id, \
             register_id, cashier_session_id, user_id, approved_by, reverses, local_day, seq, \
             posted_at, request_hash, created_at) SELECT X'00000000000070008000000000009999', \
             'X-1', kind, amount, reason_id, note, branch_id, register_id, cashier_session_id, \
             user_id, approved_by, reverses, local_day, seq + 100, posted_at, request_hash, \
             created_at FROM cash_movement WHERE reverses IS NOT NULL AND kind = 'drop'"
        )
        .await
        .is_err()
    );
}

async fn register_two(db: &Db, till: &Till) -> RegisterId {
    let r = Register {
        id: RegisterId::generate(),
        branch: till.o.jkt.id,
        code: "R2".into(),
        name: "Back".into(),
        default_location: None,
        archived: false,
    };
    let mut tx = db.begin().await.unwrap();
    sultan_db::org::save_register(&mut tx, &r, t(1))
        .await
        .unwrap();
    tx.commit().await.unwrap();
    r.id
}

/// Spec 2.7 §2, §12 case 18 (the database half): archived reasons, reason direction, and the
/// session a movement needs.
async fn reason_and_session_checks(db: &Db) {
    let till = till(db).await;
    let old = mv(db, &till, CashKind::Out, 15_000, Some(PARKING), NOV16).await;
    let mut tx = db.begin().await.unwrap();
    cash::archive_cash_reason(&mut tx, SEED_REASON_IDS[PARKING], &actor(&till.o), t(NOV16))
        .await
        .unwrap();
    tx.commit().await.unwrap();
    // The rules refuse it now.
    let archived = reason(db, SEED_REASON_IDS[PARKING]).await;
    assert!(
        CashMovement::new(
            CashMovementId::generate(),
            till.session,
            CashKind::Out,
            rp(1),
            Some(&archived),
            None,
        )
        .is_err()
    );
    // So does the database, for a movement built around them.
    let raw_mv = |kind, reason: Option<CashReasonId>| CashMovement {
        id: CashMovementId::generate(),
        session: till.session,
        kind,
        amount: rp(1_000),
        reason,
        note: None,
        reverses: None,
    };
    for m in [
        raw_mv(CashKind::Out, Some(SEED_REASON_IDS[PARKING])),
        raw_mv(CashKind::Out, Some(SEED_REASON_IDS[TOP_UP])),
        raw_mv(CashKind::In, Some(CashReasonId::generate())),
        raw_mv(CashKind::In, None),
    ] {
        match post(db, &new_mv(&till, m, NOV16, 1)).await {
            Err(CashDbError::Invalid { field, .. }) => assert_eq!(field, "reason"),
            other => panic!("{other:?}"),
        }
    }
    // The old movement still reads with its reason, and may still be reversed (§16 reading 2).
    let stored = cash::get_cash_movement(db, old.movement.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.reason_name.as_deref(), Some("Ongkos kirim / parkir"));
    reverse(db, &till, &old, NOV16 + 1).await.unwrap();
    // No open session of the caller on this register.
    let safe_drop = |session| raw_mv(CashKind::Drop, None).with_session(session);
    let other_user = NewCashMovement {
        user: till.o.manager.id,
        ..new_mv(&till, safe_drop(till.session), NOV16, 1)
    };
    let unknown = new_mv(&till, safe_drop(CashierSessionId::generate()), NOV16, 1);
    for new in [other_user, unknown] {
        let err = post(db, &new).await.unwrap_err();
        assert!(matches!(err, CashDbError::NoOpenSession), "{err:?}");
        assert_eq!(err.code(), Some("no_open_session"));
    }
    let mut session = cashier_sessions::get(db, till.session)
        .await
        .unwrap()
        .unwrap()
        .session;
    let report = session
        .close(till.o.cashier.id, Money::ZERO, &[], t(NOV16 + 2))
        .unwrap();
    let mut tx = db.begin().await.unwrap();
    cashier_sessions::close(&mut tx, &session, &report, &[])
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert!(matches!(
        post(db, &new_mv(&till, safe_drop(till.session), NOV16 + 3, 1)).await,
        Err(CashDbError::NoOpenSession)
    ));
    // The schema refuses a drop with a reason.
    assert!(
        raw(
            db,
            "INSERT INTO cash_movement (id, number, kind, amount, reason_id, note, branch_id, \
             register_id, cashier_session_id, user_id, approved_by, reverses, local_day, seq, \
             posted_at, request_hash, created_at) SELECT X'00000000000070008000000000009998', \
             'X-2', 'drop', amount, reason_id, note, branch_id, register_id, \
             cashier_session_id, user_id, NULL, NULL, local_day, seq + 100, posted_at, \
             request_hash, created_at FROM cash_movement WHERE reverses IS NULL AND kind = 'out'"
        )
        .await
        .is_err()
    );
}

trait WithSession {
    fn with_session(self, session: CashierSessionId) -> Self;
}

impl WithSession for CashMovement {
    fn with_session(self, session: CashierSessionId) -> Self {
        Self { session, ..self }
    }
}

/// Spec 2.7 §2a, §11, §12 case 33 (the database half): filters, keyset paging and the
/// missing-nota flag.
#[allow(clippy::too_many_lines)] // one story per area, like the sales tests
async fn lists_filter_and_page(db: &Db) {
    let till = till(db).await;
    let small = mv(db, &till, CashKind::Out, 99_999, Some(PARKING), NOV16).await;
    let at_flag = mv(db, &till, CashKind::Out, 100_000, Some(PARKING), NOV16 + 1).await;
    let with_photo = mv(
        db,
        &till,
        CashKind::Out,
        150_000,
        Some(OTHER_OUT),
        NOV16 + 2,
    )
    .await;
    let reversed = mv(db, &till, CashKind::Out, 120_000, Some(PARKING), NOV16 + 3).await;
    let top_up = mv(db, &till, CashKind::In, 500_000, Some(TOP_UP), NOV16 + 4).await;
    let safe_drop = mv(db, &till, CashKind::Drop, 300_000, None, NOV16 + 5).await;
    reverse(db, &till, &reversed, NOV16 + 6).await.unwrap();
    add(db, &till, with_photo.movement.id, 1).await.unwrap();
    let list = |filter: CashFilter| async move {
        cash::list_cash_movements(db, &filter)
            .await
            .unwrap()
            .into_iter()
            .map(|m| m.movement.id)
            .collect::<Vec<_>>()
    };
    let all = list(CashFilter::default()).await;
    assert_eq!(all.len(), 7);
    assert_eq!(all[1], safe_drop.movement.id, "newest first");
    // Missing nota: only the cash out at the threshold without a photo and not reversed.
    let flagged = list(CashFilter {
        missing_nota_above: Some(rp(100_000)),
        ..CashFilter::default()
    })
    .await;
    assert_eq!(flagged, vec![at_flag.movement.id]);
    let flag = rp(100_000);
    let stored = |id| async move { cash::get_cash_movement(db, id).await.unwrap().unwrap() };
    assert!(stored(at_flag.movement.id).await.missing_nota(flag));
    assert!(!stored(small.movement.id).await.missing_nota(flag));
    assert!(!stored(with_photo.movement.id).await.missing_nota(flag));
    assert!(!stored(reversed.movement.id).await.missing_nota(flag));
    assert!(!stored(at_flag.movement.id).await.missing_nota(Money::ZERO));
    assert!(
        list(CashFilter {
            missing_nota_above: Some(Money::ZERO),
            ..CashFilter::default()
        })
        .await
        .is_empty()
    );
    // Lower the threshold: the small one is flagged too.
    assert_eq!(
        list(CashFilter {
            missing_nota_above: Some(rp(50_000)),
            ..CashFilter::default()
        })
        .await,
        vec![at_flag.movement.id, small.movement.id]
    );
    // Filters.
    let kind = |k| CashFilter {
        kind: Some(k),
        ..CashFilter::default()
    };
    assert_eq!(list(kind(CashKind::In)).await, vec![top_up.movement.id]);
    assert_eq!(list(kind(CashKind::Out)).await.len(), 5);
    assert_eq!(
        list(CashFilter {
            reason: Some(SEED_REASON_IDS[OTHER_OUT]),
            ..CashFilter::default()
        })
        .await,
        vec![with_photo.movement.id]
    );
    assert_eq!(
        list(CashFilter {
            from: Some(t(NOV16 + 4)),
            until: Some(t(NOV16 + 6)),
            ..CashFilter::default()
        })
        .await,
        vec![safe_drop.movement.id, top_up.movement.id]
    );
    for filter in [
        CashFilter {
            branch: Some(till.o.jkt.id),
            ..CashFilter::default()
        },
        CashFilter {
            register: Some(till.o.register.id),
            ..CashFilter::default()
        },
        CashFilter {
            cashier_session: Some(till.session),
            ..CashFilter::default()
        },
    ] {
        assert_eq!(list(filter).await.len(), 7);
    }
    assert!(
        list(CashFilter {
            branch: Some(till.o.bdg.id),
            ..CashFilter::default()
        })
        .await
        .is_empty()
    );
    // Keyset paging.
    let mut seen = Vec::new();
    let mut before = None;
    loop {
        let page = list(CashFilter {
            before,
            limit: 3,
            ..CashFilter::default()
        })
        .await;
        if page.is_empty() {
            break;
        }
        before = page.last().copied();
        seen.extend(page);
    }
    assert_eq!(seen, all);
}

fn photo(movement: CashMovementId, n: u8, user: UserId) -> NotaPhoto {
    NotaPhoto {
        id: NotaPhotoId::generate(),
        movement,
        sha256: [n; 32],
        size: 1_000 + u64::from(n),
        added_by: user,
        added_at: t(NOV16 + i64::from(n)),
    }
}

async fn add_photo(db: &Db, till: &Till, p: &NotaPhoto) -> CashResult<Written> {
    let mut tx = db.begin().await.unwrap();
    let out = cash::add_photo(&mut tx, p, &actor(&till.o), p.added_at).await;
    if out.is_ok() {
        tx.commit().await.unwrap();
    }
    out
}

async fn add(db: &Db, till: &Till, movement: CashMovementId, n: u8) -> CashResult<Written> {
    add_photo(db, till, &photo(movement, n, till.o.cashier.id)).await
}

/// Spec 2.7 §2a, §12 case 31 (the database half): replay, conflict and the three-photo limit.
async fn photos_limit_and_replay(db: &Db) {
    let till = till(db).await;
    let out = mv(db, &till, CashKind::Out, 150_000, Some(PARKING), NOV16).await;
    let id = out.movement.id;
    let first = photo(id, 1, till.o.cashier.id);
    let before = trail(db).await;
    assert_eq!(
        add_photo(db, &till, &first).await.unwrap(),
        Written::Created
    );
    assert_eq!(trail(db).await, (before.0 + 1, before.1 + 1));
    // The same photo again: nothing written; another file under the same ID: a conflict.
    assert_eq!(
        add_photo(db, &till, &first).await.unwrap(),
        Written::Unchanged
    );
    assert_eq!(trail(db).await, (before.0 + 1, before.1 + 1));
    let other = NotaPhoto {
        sha256: [7; 32],
        ..first
    };
    assert!(matches!(
        add_photo(db, &till, &other).await,
        Err(CashDbError::Conflict("nota photo", _))
    ));
    assert_eq!(add(db, &till, id, 2).await.unwrap(), Written::Created);
    assert_eq!(
        add(db, &till, id, 2).await.unwrap(),
        Written::Created,
        "same file, new id"
    );
    match add(db, &till, id, 4).await {
        Err(e @ CashDbError::Rule(CashError::PhotoLimit { max: 3 })) => {
            assert_eq!(e.code(), Some("photo_limit"));
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(cash::count_photos(db, id).await.unwrap(), 3);
    let photos = cash::list_photos(db, id).await.unwrap();
    assert_eq!(photos.len(), 3);
    assert_eq!(photos[0], first);
    assert_eq!(photos[1].sha256, [2; 32]);
    assert_eq!(cash::get_photo(db, first.id).await.unwrap(), Some(first));
    assert_eq!(
        cash::get_cash_movement(db, id)
            .await
            .unwrap()
            .unwrap()
            .photos,
        3
    );
    // Unknown movement, empty file.
    assert!(matches!(
        add(db, &till, CashMovementId::generate(), 1).await,
        Err(CashDbError::NotFound("cash movement"))
    ));
    let other_mv = mv(db, &till, CashKind::Out, 1_000, Some(PARKING), NOV16).await;
    let empty = NotaPhoto {
        size: 0,
        ..photo(other_mv.movement.id, 1, till.o.cashier.id)
    };
    assert!(matches!(
        add_photo(db, &till, &empty).await,
        Err(CashDbError::Invalid { field: "size", .. })
    ));
    // The outbox carries the photo for the background file push.
    let entries = audit::query(
        db,
        &audit::AuditFilter {
            action: Some("cash_movement.photo".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(entries.len(), 3);
    let after = entries.last().unwrap().after.as_ref().unwrap();
    assert_eq!(after["sha256"], "01".repeat(32));
    assert_eq!(after["size"], 1_001);
    // The schema caps the slots and keeps photos append-only.
    for sql in [
        "UPDATE cash_movement_photo SET size = 1",
        "DELETE FROM cash_movement_photo",
        "INSERT INTO cash_movement_photo (id, movement_id, photo_no, sha256, size, added_by, \
         added_at) SELECT X'00000000000070008000000000009997', movement_id, 4, sha256, size, \
         added_by, added_at FROM cash_movement_photo WHERE photo_no = 1",
    ] {
        assert!(raw(db, sql).await.is_err(), "{sql}");
    }
    assert_eq!(count(db, "cash_movement_photo").await, 3);
}

async fn movements_are_append_only(db: &Db) {
    let till = till(db).await;
    mv(db, &till, CashKind::Drop, 10_000, None, NOV16).await;
    for sql in [
        "UPDATE cash_movement SET amount = 1",
        "DELETE FROM cash_movement",
        "UPDATE cash_movement SET note = 'x'",
    ] {
        assert!(raw(db, sql).await.is_err(), "{sql}");
    }
    assert_eq!(count(db, "cash_movement").await, 1);
}

async fn audit_on(db: &Db, till: &Till, session: CashierSessionId, action: &str, n: usize) {
    for _ in 0..n {
        let mut entry = AuditEntry::new(t(NOV16), action);
        entry.branch = Some(till.o.jkt.id);
        entry.target = Some(format!("cashier_session:{session}"));
        audit::append(db, &entry).await.unwrap();
    }
}

/// Spec 2.7 §4, §12 cases 1 and 19 (the database half): the session's movements, photo counts,
/// drawer opens and X reads feed `core::cash::session_figures`, and the netted events feed both
/// close-report blocks.
async fn session_inputs_feed_the_figures(db: &Db) {
    let till = till(db).await;
    let top_up = mv(db, &till, CashKind::In, 50_000, Some(TOP_UP), NOV16).await;
    let out = mv(db, &till, CashKind::Out, 30_000, Some(PARKING), NOV16 + 1).await;
    let wrong = mv(db, &till, CashKind::Out, 7_000, Some(PARKING), NOV16 + 2).await;
    reverse(db, &till, &wrong, NOV16 + 3).await.unwrap();
    mv(db, &till, CashKind::Drop, 300_000, None, NOV16 + 4).await;
    add(db, &till, out.movement.id, 1).await.unwrap();
    add(db, &till, out.movement.id, 2).await.unwrap();
    audit_on(db, &till, till.session, cash::DRAWER_OPEN_ACTION, 2).await;
    audit_on(db, &till, till.session, cash::X_REPORT_ACTION, 1).await;
    audit_on(
        db,
        &till,
        CashierSessionId::generate(),
        cash::X_REPORT_ACTION,
        3,
    )
    .await;
    audit_on(db, &till, till.session, "sale.post", 1).await;

    let inputs = cash::session_cash(db, till.session).await.unwrap();
    assert_eq!(inputs.movements.len(), 5);
    assert_eq!(inputs.movements[0], top_up.movement);
    assert_eq!(inputs.photos, BTreeMap::from([(out.movement.id, 2usize)]));
    assert_eq!((inputs.drawer_opens, inputs.x_reports), (2, 1));
    assert_eq!(
        cash::session_cash(db, CashierSessionId::generate())
            .await
            .unwrap(),
        cash::SessionCash::default()
    );
    let events = cash::session_cash_events(db, till.session).await.unwrap();
    assert_eq!(
        events,
        vec![
            SessionEvent::CashIn(rp(50_000)),
            SessionEvent::CashOut(rp(30_000)),
            SessionEvent::Drop(rp(300_000)),
        ]
    );
    // The figures from the stored inputs: 200 000 + 50 000 − 30 000 − 300 000.
    let reasons: Vec<CashReason> = cash::list_cash_reasons(db, true)
        .await
        .unwrap()
        .iter()
        .map(|r| r.reason.clone())
        .collect();
    let input = SessionInput {
        opening_cash: rp(200_000),
        events: &[],
        movements: &inputs.movements,
        reasons: &reasons,
        photos: &inputs.photos,
        nota_flag_above: cash::nota_flag_above(db).await.unwrap(),
        drawer_opens: inputs.drawer_opens,
        x_reports: inputs.x_reports,
    };
    let figures = session_figures(&input).unwrap();
    assert_eq!(figures.expected_cash(), rp(-80_000));
    assert_eq!((figures.drawer_opens, figures.x_reports), (2, 1));
    assert_eq!(figures.missing_nota, 0, "the 30 000 is below the threshold");
    assert_eq!(figures.cash.reversals.count, 1);
}

/// Spec 2.7 §3, §12 cases 9–10 (the database half): the float kept at a close is proposed at
/// the register's next open; a different opening stores the proposal.
async fn float_between_sessions(db: &Db) {
    let till = till(db).await;
    let r1 = till.o.register.id;
    assert_eq!(cash::proposed_float(db, r1).await.unwrap(), Money::ZERO);
    let state = cash::session_cash_state(db, till.session)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(state, cash::SessionCashState::default());
    assert_eq!(
        cash::session_cash_state(db, CashierSessionId::generate())
            .await
            .unwrap(),
        None
    );
    // Not closed yet: the float cannot be recorded.
    let mut tx = db.begin().await.unwrap();
    assert!(
        cash::record_close_cash(&mut tx, till.session, rp(1), 0)
            .await
            .is_err()
    );
    drop(tx);
    let close = |session: CashierSessionId, at: i64, float: Option<i64>| async move {
        let mut s = cashier_sessions::get(db, session)
            .await
            .unwrap()
            .unwrap()
            .session;
        let report = s.close(s.user(), rp(1_250_000), &[], t(at)).unwrap();
        let mut tx = db.begin().await.unwrap();
        cashier_sessions::close(&mut tx, &s, &report, &[])
            .await
            .unwrap();
        if let Some(f) = float {
            cash::record_close_cash(&mut tx, session, rp(f), 1)
                .await
                .unwrap();
        }
        tx.commit().await.unwrap();
    };
    close(till.session, NOV16, Some(200_000)).await;
    let state = cash::session_cash_state(db, till.session)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(state.float_kept, Some(rp(200_000)));
    assert_eq!(state.missing_nota, Some(1));
    // Written once.
    let mut tx = db.begin().await.unwrap();
    assert!(
        cash::record_close_cash(&mut tx, till.session, rp(1), 0)
            .await
            .is_err()
    );
    drop(tx);
    assert_eq!(cash::proposed_float(db, r1).await.unwrap(), rp(200_000));
    // The next open types 150 000: the proposal is stored (difference −50 000).
    let next = open_session(db, till.o.cashier.id, r1, NOV16 + HOUR).await;
    let mut tx = db.begin().await.unwrap();
    cash::set_opening_proposed(&mut tx, next, rp(200_000))
        .await
        .unwrap();
    assert!(
        cash::set_opening_proposed(&mut tx, next, rp(1))
            .await
            .is_err(),
        "once"
    );
    tx.commit().await.unwrap();
    let state = cash::session_cash_state(db, next).await.unwrap().unwrap();
    assert_eq!(state.opening_proposed, Some(rp(200_000)));
    let mut tx = db.begin().await.unwrap();
    assert!(
        cash::set_opening_proposed(&mut tx, till.session, rp(1))
            .await
            .is_err(),
        "only an open session"
    );
    drop(tx);
    // A later close without a float (e.g. before 2.7) proposes 0; the last close counts.
    close(next, NOV16 + 2 * HOUR, None).await;
    assert_eq!(cash::proposed_float(db, r1).await.unwrap(), Money::ZERO);
    let third = open_session(db, till.o.cashier.id, r1, NOV16 + 3 * HOUR).await;
    close(third, NOV16 + 4 * HOUR, Some(1_000_000)).await;
    assert_eq!(cash::proposed_float(db, r1).await.unwrap(), rp(1_000_000));
}

/// Spec 2.7 §4: the close stores its reasons as they are, once, and only on a closed session.
async fn close_keeps_the_reasons(db: &Db) {
    let till = till(db).await;
    let parking = SEED_REASON_IDS[3];
    let line = |name: &str, direction, sort| ReasonLine {
        name: name.to_owned(),
        direction,
        sort,
        count: 1,
        amount: rp(30_000),
        reversals: 0,
    };
    let lines = BTreeMap::from([
        (parking, line("Ongkos kirim / parkir", Direction::Out, 20)),
        (
            SEED_REASON_IDS[0],
            line("Tambahan modal", Direction::In, -5),
        ),
    ]);
    assert!(
        cash::close_reasons(db, till.session)
            .await
            .unwrap()
            .is_empty()
    );
    // Open: refused.
    let mut tx = db.begin().await.unwrap();
    assert!(matches!(
        cash::record_close_reasons(&mut tx, till.session, &lines).await,
        Err(sultan_db::DbError::NotFound(_))
    ));
    drop(tx);
    let mut s = cashier_sessions::get(db, till.session)
        .await
        .unwrap()
        .unwrap()
        .session;
    let report = s.close(s.user(), rp(1), &[], t(NOV16)).unwrap();
    let mut tx = db.begin().await.unwrap();
    cashier_sessions::close(&mut tx, &s, &report, &[])
        .await
        .unwrap();
    cash::record_close_reasons(&mut tx, till.session, &lines)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let stored = cash::close_reasons(db, till.session).await.unwrap();
    assert_eq!(stored.len(), 2);
    let p = stored.iter().find(|r| r.id == parking).unwrap();
    assert_eq!(
        (p.name.as_str(), p.direction, p.sort, p.active),
        ("Ongkos kirim / parkir", Direction::Out, 20, false)
    );
    assert!(
        stored
            .iter()
            .any(|r| r.direction == Direction::In && r.sort == -5)
    );
    // Written once.
    let mut tx = db.begin().await.unwrap();
    assert!(
        cash::record_close_reasons(&mut tx, till.session, &lines)
            .await
            .is_err()
    );
    drop(tx);
    assert!(
        cash::close_reasons(db, CashierSessionId::generate())
            .await
            .unwrap()
            .is_empty()
    );
}
