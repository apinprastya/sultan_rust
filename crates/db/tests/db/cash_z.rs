//! The Z report, the days still to close and the cash permissions (spec 2.7, phase K2).

use std::borrow::Cow;
use std::path::Path;

use serde_json::json;
use sultan_core::access::{Limit, Permission};
use sultan_core::cash::{
    CashError, CloseFigures, FloatFigures, SessionSales, ZReport, ZSession, build_z, check_z_run,
};
use sultan_core::cashier_session::CashierSession;
use sultan_core::doc_number::LocalDay;
use sultan_core::ids::{BranchId, CashierSessionId, RegisterId, RoleId, UserId, ZReportId};
use sultan_core::money::Money;
use sultan_db::cash::{
    self, CashDbError, DaySession, NewZReport, PendingDay, ZFilter, ZNumber, ZReportRecord,
};
use sultan_db::org::Register;
use sultan_db::seed::{self, CASHIER_ROLE, MANAGER_ROLE, OWNER_ROLE};
use sultan_db::{Db, cashier_sessions, roles};

use crate::catalog_fixtures::trail;
use crate::common::{MySqlTestDb, count, db_tests, raw, t};
use crate::sales_fixtures::{NOV16, Till, open_session, rp, till};

db_tests!(
    z_covers_closed_sessions_of_the_day,
    days_still_to_close,
    cash_permissions_on_a_fresh_database,
);

const HOUR: i64 = 3_600_000;

fn day(key: i64) -> LocalDay {
    LocalDay::from_key(key).unwrap()
}

async fn second_register(db: &Db, till: &Till) -> RegisterId {
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

/// Closes `session` (forced by `forced_by` when set).
async fn close(db: &Db, session: CashierSessionId, at: i64, forced_by: Option<UserId>) {
    let mut s: CashierSession = cashier_sessions::get(db, session)
        .await
        .unwrap()
        .unwrap()
        .session;
    let report = match forced_by {
        Some(by) => s.force_close(by, Money::ZERO, &[], t(at)),
        None => s.close(s.user(), Money::ZERO, &[], t(at)),
    }
    .unwrap();
    let mut tx = db.begin().await.unwrap();
    cashier_sessions::close(&mut tx, &s, &report, &[])
        .await
        .unwrap();
    tx.commit().await.unwrap();
}

/// Each session counted 500 000 and kept 200 000: 300 000 to the safe.
fn close_figures() -> CloseFigures {
    CloseFigures {
        counted_cash: rp(500_000),
        float: FloatFigures {
            float_kept: rp(200_000),
            to_safe: rp(300_000),
            ..FloatFigures::default()
        },
        ..CloseFigures::default()
    }
}

fn build(number: &ZNumber, d: LocalDay, run_at: i64, sessions: &[DaySession]) -> ZReport {
    let close = close_figures();
    let sales = SessionSales {
        count: 2,
        ..SessionSales::default()
    };
    let z: Vec<ZSession<'_>> = sessions
        .iter()
        .map(|s| ZSession {
            number: &s.number,
            register: &s.register_code,
            cashier: &s.cashier_name,
            opened_at: s.opened_at,
            closed_at: s.closed_at.unwrap(),
            forced: s.forced,
            close: &close,
            missing_nota: 0,
            sales: &sales,
        })
        .collect();
    build_z(&number.number, d, number.seq, t(run_at), &z).unwrap()
}

/// Runs the Z of `d` like the server will: the checks, the number, the report, one transaction.
async fn run_z(
    db: &Db,
    till: &Till,
    d: LocalDay,
    today: LocalDay,
    at: i64,
) -> Result<ZReportRecord, CashError> {
    let mut tx = db.begin().await.unwrap();
    let all = cash::day_sessions(&mut tx, till.o.jkt.id, d).await.unwrap();
    let candidates: Vec<_> = all.iter().map(DaySession::candidate).collect();
    let covered = check_z_run(d, today, &candidates)?;
    let sessions = cash::z_candidates(&mut tx, till.o.jkt.id, d).await.unwrap();
    assert_eq!(
        sessions.iter().map(|s| s.id).collect::<Vec<_>>(),
        covered,
        "the candidates are what check_z_run covers"
    );
    let number = cash::next_z_number(&mut tx, till.o.jkt.id, d)
        .await
        .unwrap();
    let report = build(&number, d, at, &sessions);
    let new = NewZReport {
        id: ZReportId::generate(),
        branch: till.o.jkt.id,
        report,
        sessions: covered,
        figures: json!({"number": number.number, "cash_to_deposit": 1}),
        user: till.o.manager.id,
        approved_by: Some(till.o.manager.id),
        register: Some(till.o.register.id),
    };
    let stored = cash::insert_z_report(&mut tx, &new, t(at)).await.unwrap();
    tx.commit().await.unwrap();
    Ok(stored)
}

/// Spec 2.7 §6, §12 cases 11, 22–25 (the database half).
#[allow(clippy::too_many_lines)] // one story: a day, its Z, a late session and its supplementary Z
async fn z_covers_closed_sessions_of_the_day(db: &Db) {
    let till = till(db).await; // S1: the cashier on R1, opened 09:00 WIB on the 16th.
    let r2 = second_register(db, &till).await;
    let s2 = open_session(db, till.o.manager.id, r2, NOV16 + HOUR).await;
    let (d16, d17) = (day(20_261_116), day(20_261_117));
    let numbers = |sessions: &[DaySession]| {
        sessions
            .iter()
            .map(|s| s.number.clone())
            .collect::<Vec<_>>()
    };
    // Both sessions of the 16th are open: the Z is refused, listing them.
    let all = cash::day_sessions(db, till.o.jkt.id, d16).await.unwrap();
    assert_eq!(
        numbers(&all),
        ["JKT01-R1-S-261116-01", "JKT01-R2-S-261116-01"]
    );
    assert!(all.iter().all(DaySession::is_open));
    assert_eq!(
        run_z(db, &till, d16, d16, NOV16 + 2 * HOUR).await,
        Err(CashError::SessionsOpen {
            numbers: numbers(&all)
        })
    );
    assert_eq!(
        numbers(
            &cash::open_day_sessions(db, till.o.jkt.id, d16)
                .await
                .unwrap()
        ),
        numbers(&all)
    );
    // Close one, force-close the other (case 22).
    close(db, till.session, NOV16 + 2 * HOUR, None).await;
    assert_eq!(
        run_z(db, &till, d16, d16, NOV16 + 2 * HOUR).await,
        Err(CashError::SessionsOpen {
            numbers: vec!["JKT01-R2-S-261116-01".into()]
        })
    );
    close(db, s2, NOV16 + 3 * HOUR, Some(till.o.manager.id)).await;
    // A session opened on the 17th (01:00 WIB) does not block the 16th (case 23).
    let s3 = open_session(db, till.o.cashier.id, till.o.register.id, NOV16 + 15 * HOUR).await;
    assert_eq!(
        cash::day_sessions(db, till.o.jkt.id, d17).await.unwrap()[0].id,
        s3
    );
    // A future day is refused.
    assert!(matches!(
        run_z(db, &till, d17, d16, NOV16).await,
        Err(CashError::InvalidField { field: "day", .. })
    ));
    let before = trail(db).await;
    let z1 = run_z(db, &till, d16, d17, NOV16 + 16 * HOUR).await.unwrap();
    assert_eq!(trail(db).await, (before.0 + 1, before.1 + 1));
    assert_eq!(z1.number, "JKT01-Z-261116-01");
    assert_eq!((z1.business_day, z1.seq, z1.supplementary), (d16, 1, false));
    assert_eq!((z1.session_count, z1.sales_count), (2, 4));
    assert_eq!(z1.counted_cash, rp(1_000_000));
    assert_eq!(z1.cash_to_deposit, rp(600_000));
    assert_eq!(z1.figures["number"], "JKT01-Z-261116-01");
    assert_eq!(z1.approved_by, Some(till.o.manager.id));
    assert_eq!(
        cash::get_z_report(db, z1.id).await.unwrap(),
        Some(z1.clone())
    );
    assert_eq!(
        cash::z_report_sessions(db, z1.id).await.unwrap(),
        [till.session, s2]
    );
    let state = cash::session_cash_state(db, s2).await.unwrap().unwrap();
    assert_eq!(state.z_report, Some(z1.id));
    let forced = cash::day_sessions(db, till.o.jkt.id, d16).await.unwrap();
    assert!(forced[1].forced && !forced[0].forced);
    // Nothing left (case 24's third run, case 25 without the key).
    assert_eq!(
        run_z(db, &till, d16, d17, NOV16 + 16 * HOUR).await,
        Err(CashError::NothingToClose)
    );
    // A session of the 16th opened after its Z (23:00 WIB, closed 00:30 on the 17th, case 11)
    // gets a supplementary Z covering only it.
    let s4 = open_session(db, till.o.manager.id, r2, NOV16 + 13 * HOUR).await;
    close(db, s4, NOV16 + 14 * HOUR + HOUR / 2, None).await;
    let z2 = run_z(db, &till, d16, d17, NOV16 + 17 * HOUR).await.unwrap();
    assert_eq!(z2.number, "JKT01-Z-261116-02");
    assert!(z2.supplementary);
    assert_eq!(cash::z_report_sessions(db, z2.id).await.unwrap(), [s4]);
    assert_eq!(
        run_z(db, &till, d16, d17, NOV16 + 17 * HOUR).await,
        Err(CashError::NothingToClose)
    );
    // Lists.
    let list = |filter: ZFilter| async move {
        cash::list_z_reports(db, &filter)
            .await
            .unwrap()
            .into_iter()
            .map(|z| z.id)
            .collect::<Vec<_>>()
    };
    assert_eq!(list(ZFilter::default()).await, [z2.id, z1.id]);
    let jkt = |f: ZFilter| ZFilter {
        branch: Some(till.o.jkt.id),
        ..f
    };
    assert_eq!(
        list(jkt(ZFilter {
            from_day: Some(d16),
            until_day: Some(d16),
            ..ZFilter::default()
        }))
        .await
        .len(),
        2
    );
    assert!(
        list(jkt(ZFilter {
            from_day: Some(d17),
            ..ZFilter::default()
        }))
        .await
        .is_empty()
    );
    assert!(
        list(jkt(ZFilter {
            until_day: Some(day(20_261_115)),
            ..ZFilter::default()
        }))
        .await
        .is_empty()
    );
    assert_eq!(
        list(ZFilter {
            before: Some(z2.id),
            limit: 1,
            ..ZFilter::default()
        })
        .await,
        [z1.id]
    );
    assert!(
        list(ZFilter {
            branch: Some(till.o.bdg.id),
            ..ZFilter::default()
        })
        .await
        .is_empty()
    );
    lost_races(db, &till, &z1, s3).await;
}

/// What the database guards when two Z runs race: the number, and each session in one Z.
async fn lost_races(db: &Db, till: &Till, z1: &ZReportRecord, open: CashierSessionId) {
    let d16 = z1.business_day;
    let sessions = cash::day_sessions(db, till.o.jkt.id, d16).await.unwrap();
    let insert = |number: ZNumber, covered: Vec<CashierSessionId>| {
        let sessions = &sessions;
        async move {
            let new = NewZReport {
                id: ZReportId::generate(),
                branch: till.o.jkt.id,
                report: build(&number, d16, NOV16, &sessions[..1]),
                sessions: covered,
                figures: json!({}),
                user: till.o.manager.id,
                approved_by: None,
                register: None,
            };
            let mut tx = db.begin().await.unwrap();
            cash::insert_z_report(&mut tx, &new, t(NOV16)).await
        }
    };
    let fresh = ZNumber {
        seq: 9,
        number: "JKT01-Z-261116-09".into(),
    };
    let taken = ZNumber {
        seq: 1,
        number: "JKT01-Z-261116-99".into(),
    };
    for (number, covered) in [
        // The session is in a Z already.
        (fresh.clone(), vec![sessions[0].id]),
        // The session is still open.
        (fresh.clone(), vec![open]),
        // An unknown session.
        (fresh.clone(), vec![CashierSessionId::generate()]),
        // Another transaction took the sequence.
        (taken, vec![sessions[0].id]),
    ] {
        let err = insert(number, covered).await.unwrap_err();
        assert!(err.is_retryable(), "{err:?}");
    }
    assert!(matches!(
        insert(fresh, Vec::new()).await,
        Err(CashDbError::Rule(CashError::NothingToClose))
    ));
    assert_eq!(count(db, "z_report").await, 2);
    let mut tx = db.begin().await.unwrap();
    assert!(matches!(
        cash::next_z_number(&mut tx, BranchId::generate(), d16).await,
        Err(CashDbError::NotFound("branch"))
    ));
    drop(tx);
    // A Z is never edited or deleted.
    for sql in [
        "UPDATE z_report SET cash_to_deposit = 0",
        "DELETE FROM z_report",
    ] {
        assert!(raw(db, sql).await.is_err(), "{sql}");
    }
}

/// Spec 2.7 K18: closed sessions without a Z, and open sessions of past days.
async fn days_still_to_close(db: &Db) {
    let till = till(db).await; // S1 open on the 16th.
    let (d15, d16, d17, d18) = (
        day(20_261_115),
        day(20_261_116),
        day(20_261_117),
        day(20_261_118),
    );
    let pending = |today| async move {
        cash::pending_days(db, till.o.jkt.id, today, None)
            .await
            .unwrap()
    };
    // Today with only an open session is not pending; tomorrow it is.
    assert!(pending(d16).await.is_empty());
    assert_eq!(
        pending(d17).await,
        [PendingDay {
            day: d16,
            open: 1,
            closed: 0
        }]
    );
    // A closed session of the 15th and of the 16th.
    let r2 = second_register(db, &till).await;
    let s15 = open_session(db, till.o.manager.id, r2, NOV16 - 24 * HOUR).await;
    close(db, s15, NOV16 - 20 * HOUR, None).await;
    let s16 = open_session(db, till.o.manager.id, r2, NOV16 + HOUR).await;
    close(db, s16, NOV16 + 2 * HOUR, None).await;
    assert_eq!(
        pending(d16).await,
        [
            PendingDay {
                day: d15,
                open: 0,
                closed: 1
            },
            PendingDay {
                day: d16,
                open: 1,
                closed: 1
            },
        ]
    );
    // Days before the install day of cash management are left out (§17 reading 10).
    assert_eq!(
        cash::pending_days(db, till.o.jkt.id, d16, Some(d16))
            .await
            .unwrap(),
        [PendingDay {
            day: d16,
            open: 1,
            closed: 1
        }]
    );
    // The migration recorded when it ran.
    assert!(cash::cash_since(db).await.unwrap().is_some());
    // The Z of the 15th (a catch-up) takes it off the list.
    let z = run_z(db, &till, d15, d16, NOV16).await.unwrap();
    assert_eq!(z.number, "JKT01-Z-261115-01");
    assert_eq!(pending(d16).await.len(), 1);
    // Another branch has nothing.
    assert!(
        cash::pending_days(db, till.o.bdg.id, d18, None)
            .await
            .unwrap()
            .is_empty()
    );
}

/// A role's `cash.out` grant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Out {
    Missing,
    Unlimited,
    Max(i64),
}

fn cash_grants(role: &sultan_core::access::Role) -> (bool, bool, Out) {
    let has = |p| role.grants.iter().any(|g| g.permission() == p);
    let out = match role
        .grants
        .iter()
        .find(|g| g.permission() == Permission::CashOut)
        .map(sultan_core::access::Grant::limit)
    {
        None => Out::Missing,
        Some(None) => Out::Unlimited,
        Some(Some(Limit::MaxAmount(m))) => Out::Max(m.minor()),
        Some(Some(other)) => panic!("{other:?}"),
    };
    (has(Permission::CashDrop), has(Permission::DayClose), out)
}

async fn grants_of(db: &Db, role: RoleId) -> (bool, bool, Out) {
    cash_grants(&roles::get_role(db, role).await.unwrap().unwrap())
}

const fn amount(n: i64) -> Out {
    Out::Max(n)
}

/// Spec 2.7 §7: the seeded roles of a fresh install.
async fn cash_permissions_on_a_fresh_database(db: &Db) {
    for now in [t(1), t(2)] {
        seed::seed_defaults(db, now).await.unwrap();
        assert_eq!(
            grants_of(db, OWNER_ROLE).await,
            (true, true, Out::Unlimited)
        );
        assert_eq!(
            grants_of(db, MANAGER_ROLE).await,
            (true, true, amount(2_000_000))
        );
        assert_eq!(
            grants_of(db, CASHIER_ROLE).await,
            (false, false, amount(50_000))
        );
    }
}

const CASH_MIGRATION: i64 = 20_261_119_000_001;

async fn old_migrator(backend: &str) -> sqlx::migrate::Migrator {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("migrations")
        .join(backend);
    let mut migrator = sqlx::migrate::Migrator::new(dir).await.unwrap();
    let old: Vec<_> = migrator
        .migrations
        .iter()
        .filter(|m| m.version < CASH_MIGRATION)
        .cloned()
        .collect();
    migrator.migrations = Cow::Owned(old);
    migrator
}

/// An install from before 2.7: the migration grants `cash.drop` and `day.close` to Manager, and
/// `cash.out` ≤ Rp 50 000 to Cashier only when Cashier has no `cash.out` yet (`edited`: an
/// existing grant of Rp 10 000 stays). Old sessions get empty cash columns, the company the
/// default nota threshold, and the seeded reasons appear.
async fn upgrade_adds_cash(db: &Db, edited: bool) {
    seed::seed_defaults(db, t(1)).await.unwrap();
    raw(
        db,
        "DELETE FROM role_permission WHERE perm IN ('cash.drop', 'day.close') \
         OR (perm = 'cash.out' AND role_id = X'00000000000070008000000000000003')",
    )
    .await
    .unwrap();
    if edited {
        raw(
            db,
            "INSERT INTO role_permission (role_id, perm, limit_value) \
             VALUES (X'00000000000070008000000000000003', 'cash.out', 10000)",
        )
        .await
        .unwrap();
    }
    assert_eq!(
        grants_of(db, MANAGER_ROLE).await,
        (false, false, amount(2_000_000))
    );
    let before = if edited { amount(10_000) } else { Out::Missing };
    assert_eq!(grants_of(db, CASHIER_ROLE).await, (false, false, before));
    let o = crate::common::org(db).await;
    let old = open_session(db, o.cashier.id, o.register.id, NOV16).await;

    db.migrate().await.unwrap();
    assert_eq!(
        grants_of(db, MANAGER_ROLE).await,
        (true, true, amount(2_000_000))
    );
    let cashier = if edited { 10_000 } else { 50_000 };
    assert_eq!(
        grants_of(db, CASHIER_ROLE).await,
        (false, false, amount(cashier))
    );
    // Owner gets the new keys from the seed, which rewrites it on every start (§16 reading 1).
    seed::seed_defaults(db, t(2)).await.unwrap();
    assert_eq!(
        grants_of(db, OWNER_ROLE).await,
        (true, true, Out::Unlimited)
    );
    assert_eq!(
        grants_of(db, CASHIER_ROLE).await,
        (false, false, amount(cashier)),
        "the seed leaves an existing Cashier alone"
    );
    assert_eq!(
        cash::session_cash_state(db, old).await.unwrap(),
        Some(cash::SessionCashState::default())
    );
    assert_eq!(cash::nota_flag_above(db).await.unwrap(), rp(100_000));
    assert_eq!(count(db, "cash_reason").await, 6);
}

async fn sqlite_upgrade(edited: bool) {
    let db = Db::connect("sqlite::memory:").await.unwrap();
    let Db::Sqlite(pool) = &db else {
        unreachable!()
    };
    old_migrator("sqlite").await.run(pool).await.unwrap();
    upgrade_adds_cash(&db, edited).await;
}

async fn mysql_upgrade(edited: bool) {
    let Some(test_db) = MySqlTestDb::create_empty().await else {
        return;
    };
    let Db::MySql(pool) = &test_db.db else {
        unreachable!()
    };
    old_migrator("mysql").await.run(pool).await.unwrap();
    upgrade_adds_cash(&test_db.db, edited).await;
    test_db.drop().await;
}

#[tokio::test]
async fn sqlite_upgrade_adds_cash() {
    sqlite_upgrade(false).await;
}

#[tokio::test]
async fn sqlite_upgrade_keeps_an_edited_cashier_cash_out() {
    sqlite_upgrade(true).await;
}

#[tokio::test]
async fn mysql_upgrade_adds_cash() {
    mysql_upgrade(false).await;
}

#[tokio::test]
async fn mysql_upgrade_keeps_an_edited_cashier_cash_out() {
    mysql_upgrade(true).await;
}
