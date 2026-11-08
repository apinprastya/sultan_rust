//! Cashier sessions over HTTP (spec 2.1 §5, §9 cases 9 and 16; plan Phase 6): open, one open
//! session per register and per user, blind close, close report, force close.

use std::sync::Arc;

use axum::http::{Method, StatusCode};
use serde_json::{Value, json};
use sultan_core::access::BranchScope;
use sultan_core::cashier_session::{Movement, Tender};
use sultan_core::ids::CashierSessionId;
use sultan_core::money::Money;
use sultan_db::Tx;
use sultan_db::audit::{self, AuditFilter};
use sultan_db::seed;
use sultan_server::movements::{MovementsFuture, SessionMovements};
use sultan_server::router;

use crate::common::{ANI_PIN, BUDI_PASSWORD, BUDI_PIN, Harness, Response, api_tests, new_id};

api_tests!(
    open_and_one_open_session_per_register_and_user,
    open_needs_a_register_session_and_permission,
    close_report_math,
    close_report_with_mixed_tenders,
    blind_close_hides_expected_cash,
    close_rules,
    force_close_needs_permission_at_the_branch_and_is_audited,
);

const OPEN: &str = "/api/v1/cashier-sessions";
const CURRENT: &str = "/api/v1/cashier-sessions/current";

fn close_uri(id: &str) -> String {
    format!("/api/v1/cashier-sessions/{id}/close")
}

fn force_uri(id: &str) -> String {
    format!("/api/v1/cashier-sessions/{id}/force-close")
}

/// A fixed list of movements: the sales phase's seam filled in by the test.
#[derive(Debug)]
struct Fixed(Vec<Movement>);

impl SessionMovements for Fixed {
    fn movements<'a>(&'a self, _tx: &'a mut Tx, _session: CashierSessionId) -> MovementsFuture<'a> {
        let movements = self.0.clone();
        Box::pin(async move { Ok(movements) })
    }
}

fn m(minor: i64) -> Money {
    Money::from_minor(minor)
}

/// Cash 160k sales − 20k refunds + 30k in − 15k out, plus card and QRIS (as in `core`'s case 8).
fn mixed() -> Vec<Movement> {
    use Movement::{CashIn, CashOut, Refund, Sale};
    vec![
        Sale {
            tender: Tender::Cash,
            amount: m(100_000),
        },
        Sale {
            tender: Tender::Card,
            amount: m(250_000),
        },
        Sale {
            tender: Tender::Qris,
            amount: m(40_000),
        },
        Sale {
            tender: Tender::Cash,
            amount: m(60_000),
        },
        Refund {
            tender: Tender::Cash,
            amount: m(20_000),
        },
        Refund {
            tender: Tender::Card,
            amount: m(50_000),
        },
        CashIn(m(30_000)),
        CashOut(m(15_000)),
    ]
}

async fn open(h: &Harness, session: (&str, &str), opening: i64) -> Response {
    h.at_register(
        session,
        Method::POST,
        OPEN,
        &[],
        Some(json!({ "opening_cash": opening })),
    )
    .await
}

fn id_of(r: &Response) -> String {
    r.body["id"].as_str().unwrap().to_owned()
}

async fn open_and_one_open_session_per_register_and_user(h: &Harness) {
    let ani = h.pin_token(&h.jkt_device, &h.ani, ANI_PIN).await;
    let budi = h.pin_token(&h.jkt_device, &h.budi, BUDI_PIN).await;
    let ani_at = (ani.as_str(), h.jkt_device.as_str());
    let budi_at = (budi.as_str(), h.jkt_device.as_str());

    h.at_register(ani_at, Method::GET, CURRENT, &[], None)
        .await
        .assert_problem(404, "not_found");

    let opened = open(h, ani_at, 500_000).await;
    assert_eq!(opened.status, StatusCode::CREATED, "{}", opened.body);
    let id = id_of(&opened);
    assert_eq!(opened.body["user_id"], h.ani.id.to_string());
    assert_eq!(opened.body["register_id"], h.jkt_register.id.to_string());
    assert_eq!(opened.body["branch_id"], h.jkt.id.to_string());
    assert_eq!(opened.body["opening_cash"], 500_000);
    assert_eq!(opened.body["status"], "open");
    assert_eq!(opened.body["opened_at"], h.now().unix_millis());
    assert_eq!(h.audit_count("cashier_session.open").await, 1);

    let current = h.at_register(ani_at, Method::GET, CURRENT, &[], None).await;
    assert_eq!(
        (current.status, &current.body),
        (StatusCode::OK, &opened.body)
    );

    // Case 9: the register already has an open session (another user) …
    let r = open(h, budi_at, 100).await;
    r.assert_problem(409, "session_already_open");
    assert_eq!(
        (r.body["conflict"].as_str(), r.body["session_id"].as_str()),
        (Some("register"), Some(id.as_str()))
    );
    // … and the user already has one (on another register).
    let r2 = h.second_jkt_register().await;
    let ani_r2 = h.pin_token(&r2, &h.ani, ANI_PIN).await;
    let r = open(h, (&ani_r2, &r2), 100).await;
    r.assert_problem(409, "session_already_open");
    assert_eq!(r.body["conflict"], "user");
    // Budi is free to open on R2.
    let budi_r2 = h.pin_token(&r2, &h.budi, BUDI_PIN).await;
    assert_eq!(
        open(h, (&budi_r2, &r2), 0).await.status,
        StatusCode::CREATED
    );
    assert_eq!(h.audit_count("cashier_session.open").await, 2);

    // After a close (handover), the next cashier opens a new session.
    let closed = h
        .at_register(
            ani_at,
            Method::POST,
            &close_uri(&id),
            &[],
            Some(json!({ "counted_cash": 500_000 })),
        )
        .await;
    assert_eq!(closed.status, StatusCode::OK, "{}", closed.body);
    let budi_r1 = open(h, budi_at, 0).await;
    assert_eq!(budi_r1.status, StatusCode::CONFLICT, "budi has R2 open");
    let cici_like = h
        .add_user("eka", seed::CASHIER_ROLE, BranchScope::Branch(h.jkt.id))
        .await;
    let eka = h.pin_token(&h.jkt_device, &cici_like, ANI_PIN).await;
    assert_eq!(
        open(h, (&eka, &h.jkt_device), 0).await.status,
        StatusCode::CREATED
    );
}

async fn open_needs_a_register_session_and_permission(h: &Harness) {
    let ani = h.pin_token(&h.jkt_device, &h.ani, ANI_PIN).await;
    open(h, (&ani, &h.jkt_device), -1)
        .await
        .assert_problem(422, "invalid_field");
    // A back-office session has no register.
    let back_office = h.manager_token().await;
    h.call(
        &back_office,
        Method::POST,
        OPEN,
        Some(json!({ "opening_cash": 0 })),
    )
    .await
    .assert_problem(403, "register_session_required");
    h.call(&back_office, Method::GET, CURRENT, None)
        .await
        .assert_problem(403, "register_session_required");
    // No session at all.
    h.send(Method::POST, OPEN, &[], Some(json!({ "opening_cash": 0 })))
        .await
        .assert_problem(401, "invalid_session");
    // A Viewer at JKT01 lacks `session.open_close`.
    let viewer = h
        .add_user("vera", seed::VIEWER_ROLE, BranchScope::Branch(h.jkt.id))
        .await;
    let vera = h.pin_token(&h.jkt_device, &viewer, ANI_PIN).await;
    let r = open(h, (&vera, &h.jkt_device), 0).await;
    r.assert_problem(403, "permission_denied");
    assert_eq!(r.body["permission"], "session.open_close");
    assert_eq!(h.audit_count("cashier_session.open").await, 0);
}

/// Close math over HTTP with the stub seam (no sales yet): expected = opening cash.
async fn close_report_math(h: &Harness) {
    let ani = h.pin_token(&h.jkt_device, &h.ani, ANI_PIN).await;
    let ani_at = (ani.as_str(), h.jkt_device.as_str());
    #[rustfmt::skip]
    let cases = [
        ("exact", 500_000, json!(null), 0),
        ("over, with a denomination count", 510_000,
            json!([{ "denomination": 100_000, "quantity": 5 }, { "denomination": 10_000, "quantity": 1 }]), 10_000),
        ("short", 480_500, json!(null), -19_500),
        ("empty drawer", 0, json!([]), -500_000),
    ];
    for (name, counted, counts, over_short) in cases {
        let id = id_of(&open(h, ani_at, 500_000).await);
        let mut body = json!({ "counted_cash": counted });
        if !counts.is_null() {
            body["counts"] = counts.clone();
        }
        let r = h
            .at_register(ani_at, Method::POST, &close_uri(&id), &[], Some(body))
            .await;
        assert_eq!(r.status, StatusCode::OK, "{name}: {}", r.body);
        let report = &r.body;
        assert_eq!(report["opening_cash"], 500_000, "{name}");
        assert_eq!(report["expected_cash"], 500_000, "{name}");
        assert_eq!(report["counted_cash"], counted, "{name}");
        assert_eq!(report["over_short"], over_short, "{name}");
        assert_eq!(report["transactions"], 0, "{name}");
        assert_eq!(report["tenders"], json!([]), "{name}");
        assert_eq!(report["forced"], false, "{name}");
        assert_eq!(report["session"]["status"], "closed", "{name}");
        assert_eq!(report["session"]["close_kind"], "normal", "{name}");
        assert_eq!(report["session"]["closed_at"], h.now().unix_millis());
        let expected_counts = if counts.is_null() { json!([]) } else { counts };
        assert_eq!(report["counts"], expected_counts, "{name}");
    }
    assert_eq!(h.audit_count("cashier_session.close").await, 4);

    // Counts that do not add up are rejected before anything is closed.
    let id = id_of(&open(h, ani_at, 0).await);
    let r = h
        .at_register(
            ani_at,
            Method::POST,
            &close_uri(&id),
            &[],
            Some(json!({ "counted_cash": 10, "counts": [{ "denomination": 5, "quantity": 1 }] })),
        )
        .await;
    r.assert_problem(422, "invalid_field");
    assert_eq!(r.body["field"], "counts");
    assert_eq!(
        h.at_register(ani_at, Method::GET, CURRENT, &[], None)
            .await
            .body["id"],
        id
    );
}

/// The close report with mixed tenders, cash in/out and refunds (spec 2.1 §9 cases 8 and 16),
/// through a filled-in movements seam.
async fn close_report_with_mixed_tenders(h: &Harness) {
    let app = || {
        router(
            h.state(h.node_role)
                .with_movements(Arc::new(Fixed(mixed()))),
        )
    };
    let ani = h.pin_token(&h.jkt_device, &h.ani, ANI_PIN).await;
    let bearer = format!("Bearer {ani}");
    let headers = [
        ("authorization", bearer.as_str()),
        ("x-register-token", h.jkt_device.as_str()),
    ];
    let opened = Harness::send_to(
        app(),
        Method::POST,
        OPEN,
        &headers,
        Some(json!({ "opening_cash": 500_000 })),
    )
    .await;
    let id = id_of(&opened);
    let r = Harness::send_to(
        app(),
        Method::POST,
        &close_uri(&id),
        &headers,
        Some(json!({ "counted_cash": 650_000 })),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    let report = &r.body;
    // expected = 500 000 + 160 000 − 20 000 + 30 000 − 15 000 = 655 000
    let numbers: Vec<&Value> = [
        "opening_cash",
        "cash_sales",
        "cash_refunds",
        "cash_in",
        "cash_out",
        "expected_cash",
        "counted_cash",
        "over_short",
        "transactions",
    ]
    .iter()
    .map(|k| &report[*k])
    .collect();
    assert_eq!(
        numbers,
        [
            500_000, 160_000, 20_000, 30_000, 15_000, 655_000, 650_000, -5_000, 6
        ]
        .map(Value::from)
        .iter()
        .collect::<Vec<_>>()
    );
    assert_eq!(
        report["tenders"],
        json!([
            { "tender": "cash", "sales": 160_000, "refunds": 20_000, "net": 140_000, "count": 3 },
            { "tender": "card", "sales": 250_000, "refunds": 50_000, "net": 200_000, "count": 2 },
            { "tender": "qris", "sales": 40_000, "refunds": 0, "net": 40_000, "count": 1 },
        ])
    );
    // The stored close and the audit entry carry the same figures.
    let stored = sultan_db::cashier_sessions::stored_close(&h.db, id.parse().unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (
            stored.report.expected_cash,
            stored.report.over_short,
            stored.report.transactions
        ),
        (m(655_000), m(-5_000), 6)
    );
    let entries = audit::query(
        &h.db,
        &AuditFilter {
            action: Some("cashier_session.close".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(entries[0].after.as_ref().unwrap()["over_short"], -5_000);
    assert_eq!(entries[0].before.as_ref().unwrap()["status"], "open");
}

/// Spec 2.1 §9 case 16: expected cash is hidden before the close unless `session.view_expected`.
async fn blind_close_hides_expected_cash(h: &Harness) {
    let app = || {
        router(
            h.state(h.node_role)
                .with_movements(Arc::new(Fixed(mixed()))),
        )
    };
    let call = |token: String, method: Method, uri: String, body: Option<Value>| {
        let device = h.jkt_device.clone();
        async move {
            let bearer = format!("Bearer {token}");
            Harness::send_to(
                app(),
                method,
                &uri,
                &[("authorization", &bearer), ("x-register-token", &device)],
                body,
            )
            .await
        }
    };
    let ani = h.pin_token(&h.jkt_device, &h.ani, ANI_PIN).await;
    let budi = h.pin_token(&h.jkt_device, &h.budi, BUDI_PIN).await;

    // The cashier (no `session.view_expected`) sees no expected cash, at open or later.
    let opened = call(
        ani.clone(),
        Method::POST,
        OPEN.into(),
        Some(json!({ "opening_cash": 500_000 })),
    )
    .await;
    assert_eq!(opened.status, StatusCode::CREATED);
    assert!(
        opened.body.get("expected_cash").is_none(),
        "{}",
        opened.body
    );
    let id = id_of(&opened);
    let current = call(ani.clone(), Method::GET, CURRENT.into(), None).await;
    assert!(
        current.body.get("expected_cash").is_none(),
        "{}",
        current.body
    );

    // The manager at the same register (holds `session.view_expected`) sees it live.
    let current = call(budi, Method::GET, CURRENT.into(), None).await;
    assert_eq!(current.body["id"], id);
    assert_eq!(current.body["expected_cash"], 655_000);

    // The audit entry of the open does not leak it either.
    let entries = audit::query(
        &h.db,
        &AuditFilter {
            action: Some("cashier_session.open".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert!(
        entries[0]
            .after
            .as_ref()
            .unwrap()
            .get("expected_cash")
            .is_none()
    );

    // Only the report, after the cashier entered the count, shows it.
    let report = call(
        ani,
        Method::POST,
        close_uri(&id),
        Some(json!({ "counted_cash": 655_000 })),
    )
    .await;
    assert_eq!(report.body["expected_cash"], 655_000);
    assert_eq!(report.body["over_short"], 0);
    assert_eq!(report.body["session"]["expected_cash"], 655_000);
}

async fn close_rules(h: &Harness) {
    let ani = h.pin_token(&h.jkt_device, &h.ani, ANI_PIN).await;
    let budi = h.pin_token(&h.jkt_device, &h.budi, BUDI_PIN).await;
    let ani_at = (ani.as_str(), h.jkt_device.as_str());
    let budi_at = (budi.as_str(), h.jkt_device.as_str());
    let id = id_of(&open(h, ani_at, 100_000).await);
    let count = || Some(json!({ "counted_cash": 100_000 }));

    // Someone else's session: a normal close is refused (managers use force close).
    let r = h
        .at_register(budi_at, Method::POST, &close_uri(&id), &[], count())
        .await;
    r.assert_problem(403, "permission_denied");
    // The cashier's own session, but from another register.
    let r2 = h.second_jkt_register().await;
    let ani_r2 = h.pin_token(&r2, &h.ani, ANI_PIN).await;
    h.at_register((&ani_r2, &r2), Method::POST, &close_uri(&id), &[], count())
        .await
        .assert_problem(403, "permission_denied");
    // Unknown and malformed IDs.
    h.at_register(ani_at, Method::POST, &close_uri(&new_id()), &[], count())
        .await
        .assert_problem(404, "not_found");
    h.at_register(ani_at, Method::POST, &close_uri("nope"), &[], count())
        .await
        .assert_problem(400, "invalid_body");
    assert_eq!(h.audit_count("cashier_session.close").await, 0);

    let closed = h
        .at_register(ani_at, Method::POST, &close_uri(&id), &[], count())
        .await;
    assert_eq!(closed.status, StatusCode::OK);
    // A close cannot be repeated or undone.
    let again = h
        .at_register(ani_at, Method::POST, &close_uri(&id), &[], count())
        .await;
    again.assert_problem(409, "session_closed");
    assert_eq!(again.body["session_id"], id);
    h.at_register(ani_at, Method::GET, CURRENT, &[], None)
        .await
        .assert_problem(404, "not_found");
    assert_eq!(h.audit_count("cashier_session.close").await, 1);
}

async fn force_close_needs_permission_at_the_branch_and_is_audited(h: &Harness) {
    let ani = h.pin_token(&h.jkt_device, &h.ani, ANI_PIN).await;
    let ani_at = (ani.as_str(), h.jkt_device.as_str());
    let id = id_of(&open(h, ani_at, 200_000).await);
    let body = || Some(json!({ "counted_cash": 190_000 }));

    // The cashier lacks `session.force_close`.
    let r = h
        .at_register(ani_at, Method::POST, &force_uri(&id), &[], body())
        .await;
    r.assert_problem(403, "permission_denied");
    assert_eq!(r.body["permission"], "session.force_close");
    // A manager of another branch does not hold it at JKT01.
    let dedi = h
        .add_user("dedi", seed::MANAGER_ROLE, BranchScope::Branch(h.bdg.id))
        .await;
    let dedi_token = h.password_token(&dedi.username, BUDI_PASSWORD).await;
    h.call(&dedi_token, Method::POST, &force_uri(&id), body())
        .await
        .assert_problem(403, "permission_denied");
    h.call(&dedi_token, Method::POST, &force_uri(&new_id()), body())
        .await
        .assert_problem(404, "not_found");
    assert_eq!(h.audit_count("cashier_session.force_close").await, 0);

    // The JKT01 manager, from back office.
    let budi = h.manager_token().await;
    let r = h.call(&budi, Method::POST, &force_uri(&id), body()).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert_eq!(r.body["forced"], true);
    assert_eq!(r.body["over_short"], -10_000);
    assert_eq!(r.body["session"]["close_kind"], "forced");
    assert_eq!(r.body["session"]["forced_by"], h.budi.id.to_string());
    assert_eq!(r.body["session"]["user_id"], h.ani.id.to_string());

    let entries = audit::query(
        &h.db,
        &AuditFilter {
            action: Some("cashier_session.force_close".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(entries.len(), 1);
    let entry = &entries[0];
    assert_eq!(entry.user, Some(h.budi.id));
    assert_eq!(entry.branch, Some(h.jkt.id));
    assert_eq!(entry.register, Some(h.jkt_register.id));
    assert_eq!(
        entry.target.as_deref(),
        Some(format!("cashier_session:{id}").as_str())
    );
    assert_eq!(entry.after.as_ref().unwrap()["forced"], true);

    // Already closed.
    h.call(&budi, Method::POST, &force_uri(&id), body())
        .await
        .assert_problem(409, "session_closed");
    // The cashier can open again.
    assert_eq!(open(h, ani_at, 0).await.status, StatusCode::CREATED);
}
