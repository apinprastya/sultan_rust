//! The Z report over HTTP (spec 2.7 §6, K13–K19; phase K3): spec 2.7 §12 server cases 22–26, 27
//! (a future day, HQ), 28 (one audit and one outbox row), 30 (`day.closed`), the days still to
//! close (K18, §17 reading 10), reads, lists and the text with its audited copy.

#![allow(clippy::too_many_lines)]

use axum::http::{Method, StatusCode};
use serde_json::{Value, json};
use sultan_db::Db;
use sultan_server::events::Audience;
use sultan_server::{NodeRole, router};

use crate::common::{ANI_PIN, Harness, Response, api_tests};
use crate::sales::{CASH, approve, item, line, open, sale, sell, tender};

api_tests!(
    z_report_runs_per_business_day,
    z_tax_counts_returns_without_a_receipt
);

const HOUR: i64 = 3_600_000;
/// The business day of the harness clock's start (T0 is 15:00 WIB on 2027-01-15).
const D: &str = "2027-01-15";
const D1: &str = "2027-01-16";

/// `POST /z-reports` as a back-office session with an `Idempotency-Key`.
async fn run_z(h: &Harness, token: &str, body: Value, key: &str) -> Response {
    let bearer = format!("Bearer {token}");
    h.send(
        Method::POST,
        "/api/v1/z-reports",
        &[("authorization", &bearer), ("idempotency-key", key)],
        Some(body),
    )
    .await
}

fn day(h: &Harness, day: &str) -> Value {
    json!({ "day": day, "branch_id": h.jkt.id.to_string() })
}

async fn number_of(h: &Harness, token: &str, session: &str) -> String {
    let r = h
        .call(
            token,
            Method::GET,
            &format!("/api/v1/cashier-sessions/{session}"),
            None,
        )
        .await;
    r.body["session"]["number"].as_str().unwrap().to_owned()
}

async fn force_close(h: &Harness, token: &str, session: &str, counted: i64) {
    let r = h
        .call(
            token,
            Method::POST,
            &format!("/api/v1/cashier-sessions/{session}/force-close"),
            Some(json!({ "counted_cash": counted })),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
}

/// Sets `sales_settings.cash_since` (unix ms) directly.
async fn set_cash_since(db: &Db, at: i64) {
    let sql = "UPDATE sales_settings SET cash_since = ? WHERE id = 1";
    match db {
        Db::Sqlite(pool) => {
            sqlx::query(sql).bind(at).execute(pool).await.unwrap();
        }
        Db::MySql(pool) => {
            sqlx::query(sql).bind(at).execute(pool).await.unwrap();
        }
    }
}

async fn z_report_runs_per_business_day(h: &Harness) {
    let owner = h.owner_token().await;
    let manager = h.manager_token().await;
    let teh = item(h, &owner, "TEH", 11_100).await;
    let ani = h.pin_token(&h.jkt_device, &h.ani, ANI_PIN).await;
    let at = (ani.as_str(), h.jkt_device.as_str());
    let s1 = open(h, at, 100_000).await;
    let sold = sell(h, at, sale(vec![line(&teh, 1)], vec![tender(CASH, 11_100)])).await;
    let sale_id = sold["id"].as_str().unwrap().to_owned();
    let mut events = h.events.subscribe();

    // Case 22: an open session of the day blocks its Z and is listed.
    let r = run_z(h, &owner, day(h, D), "k-open").await;
    r.assert_problem(409, "sessions_open");
    assert_eq!(r.body["sessions"], json!([number_of(h, &owner, &s1).await]));
    // Case 27: a future day; bad input.
    for (body, field) in [
        (day(h, D1), "day"),
        (day(h, "2027-13-01"), "day"),
        (json!({ "day": D }), "branch_id"),
        (json!({ "day": D, "branch_id": "nope" }), ""),
    ] {
        let key = format!("k-bad-{body}");
        let r = run_z(h, &owner, body.clone(), &key).await;
        if field.is_empty() {
            r.assert_problem(400, "invalid_body");
        } else {
            r.assert_problem(422, "invalid_field");
            assert_eq!(r.body["field"], field, "{body}");
        }
    }
    // An HQ node holds the synced copy only.
    let hq = router(h.state(NodeRole::Hq));
    let bearer = format!("Bearer {owner}");
    Harness::send_to(
        hq,
        Method::POST,
        "/api/v1/z-reports",
        &[("authorization", &bearer)],
        Some(day(h, D)),
    )
    .await
    .assert_problem(403, "read_only_replica");
    // A cashier at the till needs a manager for `day.close`.
    let at_till = |token: Option<String>, key: &'static str| {
        let ani = ani.clone();
        async move {
            let mut headers = vec![("idempotency-key", key.to_owned())];
            if let Some(t) = token {
                headers.push(("x-override", t));
            }
            let headers: Vec<(&str, &str)> =
                headers.iter().map(|(k, v)| (*k, v.as_str())).collect();
            h.at_register(
                (&ani, &h.jkt_device),
                Method::POST,
                "/api/v1/z-reports",
                &headers,
                Some(json!({ "day": D })),
            )
            .await
        }
    };
    let r = at_till(None, "k-till").await;
    r.assert_problem(403, "needs_override");
    assert_eq!(r.body["permission"], "day.close");
    // The back office's branch must be the register's.
    let r = h
        .at_register(
            at,
            Method::POST,
            "/api/v1/z-reports",
            &[],
            Some(json!({ "day": D, "branch_id": h.bdg.id.to_string() })),
        )
        .await;
    r.assert_problem(422, "invalid_field");
    // A manager force-closes the abandoned session; the Z runs at the till with approval.
    force_close(h, &manager, &s1, 111_100).await;
    let (outbox, audits) = (h.outbox_count().await, h.audit_count("z_report.post").await);
    let context = format!("{}:{D}", h.jkt.id);
    let t = approve(h, at, "day.close", Value::Null, &context).await;
    let z1 = at_till(Some(t.clone()), "z-1").await;
    assert_eq!(z1.status, StatusCode::CREATED, "{}", z1.body);
    let report = &z1.body["report"];
    assert_eq!(report["number"], "JKT01-Z-270115-01");
    assert_eq!(
        (report["day"].as_str(), report["supplementary"].as_bool()),
        (Some(D), Some(false))
    );
    assert_eq!(z1.body["session_ids"], json!([s1]));
    assert_eq!(z1.body["approved_by"], h.budi.id.to_string());
    assert_eq!(z1.body["run_by_name"], "ANI");
    let branch = &report["branch"];
    assert_eq!(branch["sales_count"], 1);
    assert_eq!(branch["first_sale"]["number"], sold["number"]);
    assert_eq!(branch["gross_sales"], 11_100);
    assert_eq!(
        branch["tax"],
        json!([{ "rate_bp": 1100, "base": 10_000, "tax": 1_100 }])
    );
    // Default float kept = the opening cash: 11 100 to the safe = the cash to deposit.
    assert_eq!(
        (branch["float_kept"].as_i64(), branch["to_safe"].as_i64()),
        (Some(100_000), Some(11_100))
    );
    assert_eq!(branch["cash_to_deposit"], 11_100);
    assert_eq!(report["registers"][0]["register"], "R1");
    assert_eq!(report["sessions"][0]["forced"], true);
    // Case 28 and 30.
    assert_eq!(h.outbox_count().await, outbox + 1);
    assert_eq!(h.audit_count("z_report.post").await, audits + 1);
    let event = std::iter::from_fn(|| events.try_recv().ok())
        .find(|e| e.kind == "day.closed")
        .unwrap();
    assert_eq!(event.audience, Audience::Branch(h.jkt.id));
    assert_eq!(
        (event.data["day"].as_str(), event.data["branch_id"].as_str()),
        (Some(D), Some(h.jkt.id.to_string().as_str()))
    );
    // Case 25: the same key answers the same Z; another key finds nothing left.
    let replay = at_till(Some(t), "z-1").await;
    assert_eq!(
        (replay.status, &replay.body),
        (StatusCode::CREATED, &z1.body)
    );
    assert_eq!(replay.headers["idempotency-replayed"], "true");
    run_z(h, &owner, day(h, D), "z-again")
        .await
        .assert_problem(409, "nothing_to_close");
    let pending = |token: String, query: String| async move {
        crate::cash::get_as(
            h,
            (&token, &h.jkt_device),
            &format!("/api/v1/z-reports/pending{query}"),
        )
        .await
    };
    let jkt_query = format!("?branch_id={}", h.jkt.id);
    assert_eq!(
        pending(owner.clone(), jkt_query.clone()).await.body,
        json!([])
    );

    // Case 24: a session of the same day opened after its Z gets a supplementary Z.
    h.clock.advance(HOUR);
    // Sessions idle out after 30 minutes: log in again.
    let owner = h.owner_token().await;
    let manager = h.manager_token().await;
    let ani = h.pin_token(&h.jkt_device, &h.ani, ANI_PIN).await;
    let at = (ani.as_str(), h.jkt_device.as_str());
    let s2 = open(h, at, 0).await;
    let s2_close = crate::cash::close(h, at, &s2, json!({ "counted_cash": 0 })).await;
    assert_eq!(s2_close["float"]["opening_proposed"], 100_000);
    assert_eq!(s2_close["float"]["float_difference"], -100_000);
    let r = pending(manager.clone(), jkt_query.clone()).await;
    assert_eq!(
        r.body,
        json!([{ "day": D, "open": 0, "closed": 1 }]),
        "{}",
        r.body
    );
    // §17 reading 10: days before cash management was installed are not listed.
    set_cash_since(&h.db, h.now().unix_millis() + 24 * HOUR).await;
    assert_eq!(
        pending(manager.clone(), jkt_query.clone()).await.body,
        json!([])
    );
    set_cash_since(&h.db, 0).await;
    let z2 = run_z(h, &owner, day(h, D), "z-2").await;
    assert_eq!(z2.status, StatusCode::CREATED, "{}", z2.body);
    assert_eq!(z2.body["report"]["number"], "JKT01-Z-270115-02");
    assert_eq!(z2.body["report"]["supplementary"], true);
    assert_eq!(z2.body["session_ids"], json!([s2]));
    assert_eq!(z2.body["report"]["branch"]["float_difference"], -100_000);
    run_z(h, &owner, day(h, D), "z-3")
        .await
        .assert_problem(409, "nothing_to_close");

    // Case 23: a session of the next day does not block the Z of this one.
    h.clock.advance(11 * HOUR);
    let owner = h.owner_token().await;
    let manager = h.manager_token().await;
    let ani = h.pin_token(&h.jkt_device, &h.ani, ANI_PIN).await;
    let at = (ani.as_str(), h.jkt_device.as_str());
    let s3 = open(h, at, 50_000).await;
    run_z(h, &owner, day(h, D), "z-4")
        .await
        .assert_problem(409, "nothing_to_close");
    run_z(h, &owner, day(h, D1), "z-5")
        .await
        .assert_problem(409, "sessions_open");
    let r = pending(owner.clone(), jkt_query.clone()).await;
    assert_eq!(r.body, json!([]), "today's open session is not pending yet");
    // Case 26: a post-void on the next day of a sale of this day.
    let t = approve(h, at, "sale.post_void", Value::Null, &sale_id).await;
    let r = h
        .at_register(
            at,
            Method::POST,
            &format!("/api/v1/sales/{sale_id}/void"),
            &[("x-override", &t), ("idempotency-key", "void-1")],
            Some(json!({ "reason": "wrong_item", "refund": "cash" })),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    crate::cash::close(h, at, &s3, json!({ "counted_cash": 38_900 })).await;
    let z3 = run_z(h, &owner, day(h, D1), "z-6").await;
    assert_eq!(z3.status, StatusCode::CREATED, "{}", z3.body);
    let b = &z3.body["report"]["branch"];
    assert_eq!(
        b["sales"]["post_voids"],
        json!({ "count": 1, "amount": 11_100 })
    );
    assert_eq!(
        b["tax"],
        json!([{ "rate_bp": 1100, "base": -10_000, "tax": -1_100 }])
    );
    assert_eq!(b["sales_count"], 0);
    let r = h
        .call(
            &owner,
            Method::GET,
            &format!("/api/v1/z-reports/{}", z1.body["id"].as_str().unwrap()),
            None,
        )
        .await;
    assert_eq!(
        r.body, z1.body,
        "the Z of the sale's day reads back unchanged"
    );

    // Lists and reads.
    let list = |token: String, query: String| async move {
        h.call(
            &token,
            Method::GET,
            &format!("/api/v1/z-reports{query}"),
            None,
        )
        .await
    };
    let r = list(manager.clone(), format!("?branch_id={}", h.jkt.id)).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    let numbers: Vec<&str> = r.body["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|z| z["number"].as_str().unwrap())
        .collect();
    assert_eq!(
        numbers,
        [
            "JKT01-Z-270116-01",
            "JKT01-Z-270115-02",
            "JKT01-Z-270115-01"
        ]
    );
    assert_eq!(r.body["items"][2]["cash_to_deposit"], 11_100);
    let r = list(owner.clone(), format!("?from={D1}&until={D1}&limit=1")).await;
    assert_eq!(r.body["items"].as_array().unwrap().len(), 1);
    assert!(r.body["next_before_id"].is_string());
    let next = r.body["next_before_id"].as_str().unwrap().to_owned();
    let r = list(owner.clone(), format!("?from={D1}&before_id={next}")).await;
    assert_eq!(r.body["items"], json!([]));
    list(manager.clone(), String::new())
        .await
        .assert_problem(403, "permission_denied");
    list(owner.clone(), "?from=yesterday".to_owned())
        .await
        .assert_problem(422, "invalid_field");
    list(owner.clone(), "?limit=500".to_owned())
        .await
        .assert_problem(422, "invalid_field");
    let z1_id = z1.body["id"].as_str().unwrap();
    let z2_id = z2.body["id"].as_str().unwrap();
    // Whoever ran a Z reads it; other cashiers do not.
    assert_eq!(
        crate::cash::get_as(h, at, &format!("/api/v1/z-reports/{z1_id}"))
            .await
            .status,
        StatusCode::OK
    );
    let cici = h.pin_token(&h.bdg_device, &h.cici, ANI_PIN).await;
    crate::cash::get_as(
        h,
        (&cici, &h.bdg_device),
        &format!("/api/v1/z-reports/{z1_id}"),
    )
    .await
    .assert_problem(403, "permission_denied");
    h.call(
        &owner,
        Method::GET,
        &format!("/api/v1/z-reports/{}", crate::common::new_id()),
        None,
    )
    .await
    .assert_problem(404, "not_found");

    // The text, and its audited copy.
    let r = h
        .call(
            &owner,
            Method::GET,
            &format!("/api/v1/z-reports/{z2_id}/text?copy=true&width=42"),
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    let text = r.body["text"].as_str().unwrap();
    for part in [
        "Z REPORT",
        "SUPPLEMENTARY",
        "COPY",
        "JKT01-Z-270115-02",
        "CASH TO DEPOSIT",
    ] {
        assert!(text.contains(part), "{part}: {text}");
    }
    assert_eq!(h.audit_count("z_report.reprint").await, 1);
    let r = h
        .at_register(
            at,
            Method::GET,
            &format!("/api/v1/z-reports/{z1_id}/text?lang=id"),
            &[],
            None,
        )
        .await;
    assert!(
        r.body["text"].as_str().unwrap().contains("LAPORAN Z"),
        "{}",
        r.body
    );
    crate::cash::get_as(h, at, &format!("/api/v1/z-reports/{z1_id}/text?copy=true"))
        .await
        .assert_problem(403, "permission_denied");

    // Pending: a register's own branch by default; needs `report.sales` or `day.close`.
    pending(ani.clone(), String::new())
        .await
        .assert_problem(403, "permission_denied");
    pending(manager.clone(), String::new())
        .await
        .assert_problem(422, "invalid_field");
    let budi = h
        .pin_token(&h.jkt_device, &h.budi, crate::common::BUDI_PIN)
        .await;
    let r = h
        .at_register(
            (&budi, &h.jkt_device),
            Method::GET,
            "/api/v1/z-reports/pending",
            &[],
            None,
        )
        .await;
    assert_eq!((r.status, &r.body), (StatusCode::OK, &json!([])));
}

/// Spec 2.7 §18 reading 13 (decided 2026-11-21): a return without a receipt enters the Z's tax
/// summary at the returned product's tax class, at the rate valid on the return's day.
async fn z_tax_counts_returns_without_a_receipt(h: &Harness) {
    let owner = h.owner_token().await;
    let teh = item(h, &owner, "TEH", 11_100).await;
    // A rate that starts tomorrow does not apply to today's return.
    let uri = format!(
        "/api/v1/tax-classes/{}/rates",
        crate::catalog_fixtures::STANDARD
    );
    let r = h
        .call(
            &owner,
            Method::PUT,
            &uri,
            Some(json!({ "rate_bp": 1200, "valid_from": D1 })),
        )
        .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
    let ani = h.pin_token(&h.jkt_device, &h.ani, ANI_PIN).await;
    let at = (ani.as_str(), h.jkt_device.as_str());
    let session = open(h, at, 0).await;
    sell(h, at, sale(vec![line(&teh, 2)], vec![tender(CASH, 22_200)])).await;
    let id = crate::common::new_id();
    let t = approve(h, at, "refund.no_receipt", json!(11_100), &id).await;
    let r = h
        .at_register(
            at,
            Method::PUT,
            &format!("/api/v1/returns/{id}"),
            &[("x-override", &t)],
            Some(json!({
                "refund": "cash", "reason": "tanpa struk",
                "lines": [{ "variant_id": teh.variant, "unit_id": teh.pcs, "qty_thousandths": 1000 }],
            })),
        )
        .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
    crate::cash::close(h, at, &session, json!({ "counted_cash": 11_100 })).await;
    let z = run_z(h, &owner, day(h, D), "z-tax").await;
    assert_eq!(z.status, StatusCode::CREATED, "{}", z.body);
    assert_eq!(
        z.body["report"]["branch"]["tax"],
        json!([{ "rate_bp": 1100, "base": 10_000, "tax": 1_100 }])
    );
}
