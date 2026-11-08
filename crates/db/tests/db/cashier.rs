//! Cashier sessions: open, close, force close, the open-session safety net, session numbers
//! (Phase 9), the stored close report, listing, and the migration backfill.

use std::path::Path;

use sqlx::migrate::Migrator;
use sultan_core::cashier_session::{
    CashierSession, CloseKind, Movement, SessionError, SessionStatus, Tender, TenderTotals,
};
use sultan_core::ids::{BranchId, CashierSessionId, RegisterId, UserId};
use sultan_core::money::Money;
use sultan_db::cashier_sessions::{
    self, DenominationCount, ReportContext, SessionFilter, SessionRecord, Visibility,
};
use sultan_db::org::Register;
use sultan_db::{Db, DbError, Tx, org, users};

use crate::common::{MySqlTestDb, Org, db_tests, org, raw, t, user};

db_tests!(
    open_close_round_trip,
    close_rejects_invalid_state,
    force_close_round_trip,
    open_session_uniqueness_safety_net,
    numbers_follow_the_branch_local_day,
    numbers_grow_past_99,
    numbers_reject_bad_stored_state,
    racing_opens_on_one_register,
    list_filters_and_pages,
    list_visibility,
);

fn rp(n: i64) -> Money {
    Money::from_minor(n)
}

/// 2026-11-07T00:00:00Z.
const NOV7_UTC: i64 = 1_794_009_600_000;
const HOUR: i64 = 3_600_000;

fn open_at(user: UserId, register: RegisterId, opening: i64, at: i64) -> CashierSession {
    CashierSession::open(
        CashierSessionId::generate(),
        user,
        register,
        rp(opening),
        t(at),
        &[],
    )
    .unwrap()
}

/// Inserts in its own transaction; returns the number.
async fn insert(db: &Db, session: &CashierSession) -> Result<String, DbError> {
    let mut tx = db.begin().await?;
    let number = cashier_sessions::insert(&mut tx, session).await?;
    tx.commit().await?;
    Ok(number)
}

async fn close_in(tx: &mut Tx, session: &mut CashierSession, at: i64) {
    let report = session
        .close(session.user(), Money::ZERO, &[], t(at))
        .unwrap();
    cashier_sessions::close(tx, session, &report, &[])
        .await
        .unwrap();
}

async fn close_now(db: &Db, session: &mut CashierSession, at: i64) {
    let mut tx = db.begin().await.unwrap();
    close_in(&mut tx, session, at).await;
    tx.commit().await.unwrap();
}

async fn second_register(db: &Db, o: &Org, code: &str) -> Register {
    let register = Register {
        id: RegisterId::generate(),
        code: code.into(),
        ..o.register.clone()
    };
    let mut tx = db.begin().await.unwrap();
    org::save_register(&mut tx, &register, t(1)).await.unwrap();
    tx.commit().await.unwrap();
    register
}

async fn more_users(db: &Db, prefix: &str, n: usize) -> Vec<UserId> {
    let mut tx = db.begin().await.unwrap();
    let mut ids = Vec::new();
    for i in 0..n {
        let u = user(&format!("{prefix}{i}"));
        users::save_user(&mut tx, &u, t(1)).await.unwrap();
        ids.push(u.id);
    }
    tx.commit().await.unwrap();
    ids
}

#[allow(clippy::too_many_lines)] // one flow: open, read back, close, read the stored report
async fn open_close_round_trip(db: &Db) {
    let o = org(db).await;
    let existing = cashier_sessions::open_conflicts(db, o.cashier.id, o.register.id)
        .await
        .unwrap();
    let mut session = CashierSession::open(
        CashierSessionId::generate(),
        o.cashier.id,
        o.register.id,
        rp(200_000),
        t(NOV7_UTC),
        &existing,
    )
    .unwrap();
    let number = insert(db, &session).await.unwrap();
    assert_eq!(number, "JKT01-R1-S-261107-01");
    let record = SessionRecord {
        session: session.clone(),
        number: number.clone(),
        branch: o.jkt.id,
        expected_at_close: None,
    };

    assert_eq!(
        cashier_sessions::open_for_register(db, o.register.id)
            .await
            .unwrap(),
        Some(record.clone())
    );
    assert_eq!(
        cashier_sessions::open_for_user(db, o.cashier.id)
            .await
            .unwrap(),
        Some(record.clone())
    );
    // The core check sees the stored session (spec 2.1 §9 case 9).
    let existing = cashier_sessions::open_conflicts(db, o.manager.id, o.register.id)
        .await
        .unwrap();
    assert!(matches!(
        CashierSession::open(
            CashierSessionId::generate(),
            o.manager.id,
            o.register.id,
            Money::ZERO,
            t(1_500),
            &existing
        ),
        Err(SessionError::SessionAlreadyOpen { .. })
    ));
    assert_eq!(
        cashier_sessions::stored_close(db, session.id())
            .await
            .unwrap(),
        None
    );

    let movements = [
        Movement::Sale {
            tender: Tender::Cash,
            amount: rp(50_000),
        },
        Movement::Sale {
            tender: Tender::Qris,
            amount: rp(30_000),
        },
        Movement::Refund {
            tender: Tender::Cash,
            amount: rp(5_000),
        },
        Movement::CashIn(rp(15_000)),
        Movement::CashOut(rp(10_000)),
    ];
    let report = session
        .close(o.cashier.id, rp(249_000), &movements, t(NOV7_UTC + HOUR))
        .unwrap();
    let counts = [
        DenominationCount {
            denomination: rp(100_000),
            quantity: 2,
        },
        DenominationCount {
            denomination: rp(1_000),
            quantity: 49,
        },
    ];
    let mut tx = db.begin().await.unwrap();
    cashier_sessions::close(&mut tx, &session, &report, &counts)
        .await
        .unwrap();
    tx.commit().await.unwrap();

    assert_eq!(
        cashier_sessions::get(db, session.id()).await.unwrap(),
        Some(SessionRecord {
            session: session.clone(),
            expected_at_close: Some(rp(250_000)),
            ..record
        })
    );
    // The stored report is exactly the report of the close (reprint, spec 2.1 §5).
    let stored = cashier_sessions::stored_close(db, session.id())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.report, report);
    assert_eq!(stored.report.expected_cash, rp(250_000));
    assert_eq!(stored.report.over_short, rp(-1_000));
    assert_eq!(stored.report.cash_in, rp(15_000));
    assert_eq!(
        stored.report.tenders.get(&Tender::Cash),
        Some(&TenderTotals {
            sales: rp(50_000),
            refunds: rp(5_000),
            net: rp(45_000),
            count: 2
        })
    );
    assert_eq!(stored.counts, counts.to_vec());
    assert_eq!(
        cashier_sessions::open_for_register(db, o.register.id)
            .await
            .unwrap(),
        None
    );
    assert_eq!(
        cashier_sessions::report_context(db, session.id())
            .await
            .unwrap(),
        Some(ReportContext {
            branch_code: "JKT01".into(),
            branch_name: "Toko JKT01".into(),
            timezone: "Asia/Jakarta".into(),
            register_code: "R1".into(),
            register_name: "Front".into(),
            cashier_name: "ANI".into(),
            forced_by_name: None,
        })
    );
    assert_eq!(
        cashier_sessions::report_context(db, CashierSessionId::generate())
            .await
            .unwrap(),
        None
    );
}

/// A close cannot be repeated; inserting a closed session or closing an open one are caller bugs.
async fn close_rejects_invalid_state(db: &Db) {
    let o = org(db).await;
    let mut session = open_at(o.cashier.id, o.register.id, 0, 1_000);
    insert(db, &session).await.unwrap();
    let report = session
        .close(o.cashier.id, Money::ZERO, &[], t(2_000))
        .unwrap();
    let mut tx = db.begin().await.unwrap();
    cashier_sessions::close(&mut tx, &session, &report, &[])
        .await
        .unwrap();

    let err = cashier_sessions::close(&mut tx, &session, &report, &[])
        .await
        .unwrap_err();
    assert!(matches!(err, DbError::NotFound(_)), "{err}");
    let err = cashier_sessions::insert(&mut tx, &session)
        .await
        .unwrap_err();
    assert!(matches!(err, DbError::Corrupt(_)), "{err}");
    let open = open_at(o.cashier.id, o.register.id, 0, 10_000);
    let err = cashier_sessions::close(&mut tx, &open, &report, &[])
        .await
        .unwrap_err();
    assert!(matches!(err, DbError::Corrupt(_)), "{err}");
}

async fn force_close_round_trip(db: &Db) {
    let o = org(db).await;
    let mut session = open_at(o.cashier.id, o.register.id, 100_000, 1_000);
    insert(db, &session).await.unwrap();
    let report = session
        .force_close(o.manager.id, rp(100_000), &[], t(2_000))
        .unwrap();
    let mut tx = db.begin().await.unwrap();
    cashier_sessions::close(&mut tx, &session, &report, &[])
        .await
        .unwrap();
    tx.commit().await.unwrap();

    let stored = cashier_sessions::get(db, session.id())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        stored.session.status(),
        SessionStatus::Closed {
            at: t(2_000),
            kind: CloseKind::Forced { by: o.manager.id }
        }
    );
    let close = cashier_sessions::stored_close(db, session.id())
        .await
        .unwrap()
        .unwrap();
    assert!(close.counts.is_empty());
    assert!(close.report.tenders.is_empty());
    assert_eq!(close.report.over_short, Money::ZERO);
    let context = cashier_sessions::report_context(db, session.id())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(context.forced_by_name.as_deref(), Some("BUDI"));
}

/// The database rejects a second open session per register and per user even when the `core`
/// check is bypassed (e.g. two racing requests), and allows a new one after a close.
async fn open_session_uniqueness_safety_net(db: &Db) {
    let o = org(db).await;
    let r2 = second_register(db, &o, "R2").await;

    let mut first = open_at(o.cashier.id, o.register.id, 0, 1_000);
    insert(db, &first).await.unwrap();

    let same_register = open_at(o.manager.id, o.register.id, 0, 1_000);
    let err = insert(db, &same_register).await.unwrap_err();
    assert!(matches!(err, DbError::Conflict(_)), "{err}");
    assert!(err.is_retryable());

    let same_user = open_at(o.cashier.id, r2.id, 0, 1_000);
    let err = insert(db, &same_user).await.unwrap_err();
    assert!(matches!(err, DbError::Conflict(_)), "{err}");

    // Another user on another register is fine.
    let other = open_at(o.manager.id, r2.id, 0, 1_000);
    insert(db, &other).await.unwrap();

    // After closing, both register and user are free again (handover, spec 2.1 §5).
    close_now(db, &mut first, 2_000).await;
    let next = open_at(o.cashier.id, o.register.id, 0, 3_000);
    insert(db, &next).await.unwrap();
}

/// Spec 2.1 §5 (decided 2026-11-08): the date is the branch-local day, `NN` counts per register
/// and local day.
async fn numbers_follow_the_branch_local_day(db: &Db) {
    let o = org(db).await;
    let r2 = second_register(db, &o, "R2").await;
    let users = more_users(db, "u", 6).await;
    #[rustfmt::skip]
    let steps = [
        // (user, register, open time UTC, expected number)
        (0, o.register.id, NOV7_UTC + 2 * HOUR,      "JKT01-R1-S-261107-01"),
        // 23:59:59.999 WIB on the 7th.
        (1, o.register.id, NOV7_UTC + 17 * HOUR - 1, "JKT01-R1-S-261107-02"),
        // 00:00 WIB on the 8th, still the 7th in UTC: a new day, NN restarts.
        (2, o.register.id, NOV7_UTC + 17 * HOUR,     "JKT01-R1-S-261108-01"),
        // Another register counts on its own.
        (3, r2.id,         NOV7_UTC + 18 * HOUR,     "JKT01-R2-S-261108-01"),
        (4, o.register.id, NOV7_UTC + 20 * HOUR,     "JKT01-R1-S-261108-02"),
    ];
    for (who, register, at, expected) in steps {
        let mut session = open_at(users[who], register, 0, at);
        let number = insert(db, &session).await.unwrap();
        assert_eq!(number, expected);
        let stored = cashier_sessions::get(db, session.id()).await.unwrap();
        assert_eq!(stored.unwrap().number, expected);
        close_now(db, &mut session, at + 1).await;
    }

    // Moving the branch to WIT (UTC+9): 15:00Z is already the 8th there.
    let mut jkt = o.jkt.clone();
    "Asia/Jayapura".clone_into(&mut jkt.timezone);
    let mut tx = db.begin().await.unwrap();
    org::save_branch(&mut tx, &jkt, t(2)).await.unwrap();
    tx.commit().await.unwrap();
    let session = open_at(users[5], r2.id, 0, NOV7_UTC + 15 * HOUR);
    assert_eq!(insert(db, &session).await.unwrap(), "JKT01-R2-S-261108-02");
}

async fn numbers_grow_past_99(db: &Db) {
    let o = org(db).await;
    let mut first = open_at(o.cashier.id, o.register.id, 0, NOV7_UTC);
    insert(db, &first).await.unwrap();
    // Pretend it was the 99th session of the day, to keep the test fast.
    raw(db, "UPDATE cashier_session SET seq = 99")
        .await
        .unwrap();
    close_now(db, &mut first, NOV7_UTC + 1).await;
    let next = open_at(o.cashier.id, o.register.id, 0, NOV7_UTC + 2);
    assert_eq!(insert(db, &next).await.unwrap(), "JKT01-R1-S-261107-100");
}

async fn numbers_reject_bad_stored_state(db: &Db) {
    let o = org(db).await;
    // An unknown register.
    let ghost = open_at(o.cashier.id, RegisterId::generate(), 0, 1_000);
    let err = insert(db, &ghost).await.unwrap_err();
    assert!(matches!(err, DbError::NotFound("register")), "{err}");
    assert!(!err.is_retryable());
    // A stored time zone that is not IANA (written around the API's check).
    raw(db, "UPDATE branch SET timezone = 'WIB'").await.unwrap();
    let session = open_at(o.cashier.id, o.register.id, 0, 1_000);
    let err = insert(db, &session).await.unwrap_err();
    assert!(
        matches!(&err, DbError::Corrupt(m) if m.contains("`WIB` is not an IANA time zone")),
        "{err}"
    );
}

/// Opens racing on one register: exactly one wins; every loser fails with an error that the
/// server retries (and then answers `409 session_already_open`). Real concurrency on `MySQL`
/// (a pool of 4); `SQLite` in memory has one connection, so the opens run one after another.
async fn racing_opens_on_one_register(db: &Db) {
    let o = org(db).await;
    let racers = more_users(db, "racer", 6).await;
    let register = o.register.id;
    let tasks: Vec<_> = racers
        .into_iter()
        .map(|user| {
            let db = db.clone();
            tokio::spawn(async move {
                let mut tx = db.begin().await?;
                let existing = cashier_sessions::open_conflicts(&mut tx, user, register).await?;
                let Ok(session) = CashierSession::open(
                    CashierSessionId::generate(),
                    user,
                    register,
                    Money::ZERO,
                    t(NOV7_UTC),
                    &existing,
                ) else {
                    return Ok(None);
                };
                let number = cashier_sessions::insert(&mut tx, &session).await?;
                tx.commit().await?;
                Ok::<_, DbError>(Some(number))
            })
        })
        .collect();
    let mut won = Vec::new();
    for task in tasks {
        match task.await.unwrap() {
            Ok(Some(number)) => won.push(number),
            Ok(None) => {}
            Err(e) => assert!(e.is_retryable(), "{e}"),
        }
    }
    assert_eq!(won, ["JKT01-R1-S-261107-01"]);
}

async fn list_filters_and_pages(db: &Db) {
    let o = org(db).await;
    let r2 = second_register(db, &o, "R2").await;
    let mut a = open_at(o.cashier.id, o.register.id, 0, NOV7_UTC);
    insert(db, &a).await.unwrap();
    close_now(db, &mut a, NOV7_UTC + HOUR).await;
    let b = open_at(o.cashier.id, o.register.id, 0, NOV7_UTC + 2 * HOUR);
    insert(db, &b).await.unwrap();
    let c = open_at(o.manager.id, r2.id, 0, NOV7_UTC + 3 * HOUR);
    insert(db, &c).await.unwrap();

    let all = SessionFilter::new(Visibility::All);
    #[rustfmt::skip]
    let cases: Vec<(SessionFilter, Vec<CashierSessionId>)> = vec![
        (all.clone(), vec![c.id(), b.id(), a.id()]),
        (SessionFilter { branch: Some(o.jkt.id), ..all.clone() }, vec![c.id(), b.id(), a.id()]),
        (SessionFilter { branch: Some(o.bdg.id), ..all.clone() }, vec![]),
        (SessionFilter { register: Some(r2.id), ..all.clone() }, vec![c.id()]),
        (SessionFilter { user: Some(o.cashier.id), ..all.clone() }, vec![b.id(), a.id()]),
        (SessionFilter { open: Some(true), ..all.clone() }, vec![c.id(), b.id()]),
        (SessionFilter { open: Some(false), ..all.clone() }, vec![a.id()]),
        (SessionFilter { from: Some(t(NOV7_UTC + 2 * HOUR)), ..all.clone() }, vec![c.id(), b.id()]),
        (SessionFilter { until: Some(t(NOV7_UTC + 2 * HOUR)), ..all.clone() }, vec![a.id()]),
        (SessionFilter { limit: 2, ..all.clone() }, vec![c.id(), b.id()]),
        (SessionFilter { before_id: Some(b.id()), ..all.clone() }, vec![a.id()]),
    ];
    for (filter, expected) in cases {
        let got: Vec<_> = cashier_sessions::list(db, &filter)
            .await
            .unwrap()
            .into_iter()
            .map(|r| r.session.id())
            .collect();
        assert_eq!(got, expected, "{filter:?}");
    }
    let first = cashier_sessions::list(db, &all).await.unwrap();
    assert_eq!(first[0].number, "JKT01-R2-S-261107-01");
    assert_eq!(first[0].branch, o.jkt.id);
}

async fn list_visibility(db: &Db) {
    let o = org(db).await;
    let mine = open_at(o.cashier.id, o.register.id, 0, 1_000);
    insert(db, &mine).await.unwrap();
    // A register at BDG01 with the manager's session.
    let bdg_register = Register {
        id: RegisterId::generate(),
        branch: o.bdg.id,
        code: "R1".into(),
        name: "Bandung".into(),
        default_location: None,
        archived: false,
    };
    let mut tx = db.begin().await.unwrap();
    org::save_register(&mut tx, &bdg_register, t(1))
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let theirs = open_at(o.manager.id, bdg_register.id, 0, 2_000);
    insert(db, &theirs).await.unwrap();

    let visible = |user: UserId, branches: Vec<BranchId>| {
        SessionFilter::new(Visibility::OwnAnd { user, branches })
    };
    #[rustfmt::skip]
    let cases = [
        (visible(o.cashier.id, vec![]), vec![mine.id()]),
        (visible(o.manager.id, vec![]), vec![theirs.id()]),
        (visible(o.cashier.id, vec![o.bdg.id]), vec![theirs.id(), mine.id()]),
        (visible(o.manager.id, vec![o.jkt.id, o.bdg.id]), vec![theirs.id(), mine.id()]),
        (visible(UserId::generate(), vec![o.jkt.id]), vec![mine.id()]),
    ];
    for (filter, expected) in cases {
        let got: Vec<_> = cashier_sessions::list(db, &filter)
            .await
            .unwrap()
            .into_iter()
            .map(|r| r.session.id())
            .collect();
        assert_eq!(got, expected, "{filter:?}");
    }
}

// --- Migration backfill (`20261108000003_session_number.sql`) ------------------------------

fn id(n: u128) -> String {
    format!("X'{n:032X}'")
}

fn uuid(n: u128) -> uuid::Uuid {
    uuid::Uuid::from_u128(n)
}

/// Runs the migrations up to and including `last` (the schema before Phase 9).
async fn migrate_until(db: &Db, last: i64) {
    let dir = match db {
        Db::Sqlite(_) => "migrations/sqlite",
        Db::MySql(_) => "migrations/mysql",
    };
    let mut migrator = Migrator::new(Path::new(env!("CARGO_MANIFEST_DIR")).join(dir))
        .await
        .unwrap();
    let older: Vec<_> = migrator
        .migrations
        .iter()
        .filter(|m| m.version <= last)
        .cloned()
        .collect();
    migrator.migrations = older.into();
    match db {
        Db::Sqlite(pool) => migrator.run(pool).await.unwrap(),
        Db::MySql(pool) => migrator.run(pool).await.unwrap(),
    }
}

/// Existing sessions get the number they would have got: branch-local (Asia/Jakarta) open day,
/// per-register sequence by open time. Closed ones get zero cash movements (sales did not exist).
async fn backfill(db: &Db) {
    migrate_until(db, 20_261_108_000_001).await;
    let day = NOV7_UTC;
    let closed = "INSERT INTO cashier_session (id, user_id, register_id, opening_cash, \
                  opened_at, closed_at, close_kind, counted_cash, expected_cash, over_short, \
                  transactions) VALUES";
    let open = "INSERT INTO cashier_session (id, user_id, register_id, opening_cash, opened_at) \
                VALUES";
    #[rustfmt::skip]
    let statements = [
        format!("INSERT INTO company VALUES ({}, 'PT', 1, 1)", id(1)),
        format!("INSERT INTO branch VALUES ({}, {}, 'JKT01', 'Jakarta', '', NULL, '', '', 0, 1, 1)", id(2), id(1)),
        format!("INSERT INTO register VALUES ({}, {}, 'R1', 'Front', NULL, 0, 1, 1)", id(3), id(2)),
        format!("INSERT INTO register VALUES ({}, {}, 'R2', 'Back', NULL, 0, 1, 1)", id(4), id(2)),
        format!("INSERT INTO app_user VALUES ({}, 'a', 'A', NULL, NULL, 1, 1, 1)", id(5)),
        format!("INSERT INTO app_user VALUES ({}, 'b', 'B', NULL, NULL, 1, 1, 1)", id(6)),
        // R1: 23:59:59.999 WIB on the 7th and 00:00 WIB on the 8th (closed), 01:00 WIB (open).
        format!("{closed} ({}, {}, {}, 5, {}, {}, 'normal', 5, 5, 0, 0)", id(10), id(5), id(3), day + 17 * HOUR - 1, day + 17 * HOUR - 1),
        format!("{closed} ({}, {}, {}, 5, {}, {}, 'normal', 5, 5, 0, 0)", id(11), id(5), id(3), day + 17 * HOUR, day + 17 * HOUR + 1),
        format!("{open} ({}, {}, {}, 5, {})", id(12), id(5), id(3), day + 18 * HOUR),
        // R2, same local day.
        format!("{open} ({}, {}, {}, 5, {})", id(13), id(6), id(4), day + 18 * HOUR),
    ];
    for sql in &statements {
        raw(db, sql).await.unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
    db.migrate().await.unwrap();

    let session = |n: u128| CashierSessionId::from(uuid(n));
    #[rustfmt::skip]
    let expected = [
        (10, "JKT01-R1-S-261107-01"),
        (11, "JKT01-R1-S-261108-01"),
        (12, "JKT01-R1-S-261108-02"),
        (13, "JKT01-R2-S-261108-01"),
    ];
    for (n, number) in expected {
        let record = cashier_sessions::get(db, session(n))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(record.number, number);
    }
    let stored = cashier_sessions::stored_close(db, session(11))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (
            stored.report.cash_sales,
            stored.report.cash_out,
            stored.report.expected_cash
        ),
        (Money::ZERO, Money::ZERO, rp(5))
    );
    let branch = org::get_branch(db, BranchId::from(uuid(2)))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(branch.timezone, "Asia/Jakarta");

    // New sessions continue the backfilled sequence.
    let mut tx = db.begin().await.unwrap();
    let mut open = cashier_sessions::get(&mut tx, session(12))
        .await
        .unwrap()
        .unwrap()
        .session;
    close_in(&mut tx, &mut open, day + 19 * HOUR).await;
    let next = CashierSession::open(
        session(20),
        UserId::from(uuid(5)),
        RegisterId::from(uuid(3)),
        Money::ZERO,
        t(day + 19 * HOUR),
        &[],
    )
    .unwrap();
    let number = cashier_sessions::insert(&mut tx, &next).await.unwrap();
    assert_eq!(number, "JKT01-R1-S-261108-03");
    tx.rollback().await.unwrap();
}

#[tokio::test]
async fn backfill_sqlite() {
    let db = Db::connect("sqlite::memory:").await.unwrap();
    backfill(&db).await;
}

#[tokio::test]
async fn backfill_mysql() {
    if let Some(test_db) = MySqlTestDb::create_empty().await {
        backfill(&test_db.db).await;
        test_db.drop().await;
    }
}
