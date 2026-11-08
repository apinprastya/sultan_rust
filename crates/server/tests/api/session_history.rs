//! Phase 9 (decided 2026-11-08): cashier session numbers in the branch's local time zone, the
//! branch `timezone`, past sessions and their stored close reports
//! (`GET /cashier-sessions/{id}`, `GET /cashier-sessions`), and racing opens.

use std::sync::Arc;

use axum::http::{Method, StatusCode};
use serde_json::{Value, json};
use sultan_core::access::BranchScope;
use sultan_core::cashier_session::{Movement, Tender};
use sultan_core::ids::CashierSessionId;
use sultan_core::money::Money;
use sultan_db::{Db, Tx, seed};
use sultan_server::movements::{MovementsFuture, SessionMovements};
use sultan_server::router;

use crate::common::{ANI_PIN, BUDI_PASSWORD, BUDI_PIN, Harness, Response, api_tests};

api_tests!(
    numbers_use_the_branch_local_day,
    branch_timezone_is_validated,
    get_shows_the_session_and_its_stored_report,
    list_filters_pages_and_visibility,
    list_and_get_reject_bad_input,
    racing_opens_get_one_session,
);

const OPEN: &str = "/api/v1/cashier-sessions";
const LIST: &str = "/api/v1/cashier-sessions";
const HOUR: i64 = 3_600_000;
/// Less than the harness's 30-minute idle expiry, so tokens stay valid.
const MINUTES_10: i64 = 600_000;

fn session_uri(id: &str) -> String {
    format!("/api/v1/cashier-sessions/{id}")
}

fn close_uri(id: &str) -> String {
    format!("/api/v1/cashier-sessions/{id}/close")
}

/// Sales: cash 100k, card 50k; a 10k cash refund.
#[derive(Debug)]
struct Fixed;

impl SessionMovements for Fixed {
    fn movements<'a>(&'a self, _tx: &'a mut Tx, _session: CashierSessionId) -> MovementsFuture<'a> {
        let m = Money::from_minor;
        Box::pin(async move {
            Ok(vec![
                Movement::Sale {
                    tender: Tender::Cash,
                    amount: m(100_000),
                },
                Movement::Sale {
                    tender: Tender::Card,
                    amount: m(50_000),
                },
                Movement::Refund {
                    tender: Tender::Cash,
                    amount: m(10_000),
                },
            ])
        })
    }
}

async fn open(h: &Harness, session: (&str, &str), opening: i64) -> Response {
    let r = h
        .at_register(
            session,
            Method::POST,
            OPEN,
            &[],
            Some(json!({ "opening_cash": opening })),
        )
        .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
    r
}

async fn close(h: &Harness, session: (&str, &str), id: &str, counted: i64) -> Response {
    let r = h
        .at_register(
            session,
            Method::POST,
            &close_uri(id),
            &[],
            Some(json!({ "counted_cash": counted })),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    r
}

fn id_of(r: &Response) -> String {
    r.body["id"].as_str().unwrap().to_owned()
}

/// The harness clock starts at 2027-01-15T08:00Z = 15:00 WIB.
async fn numbers_use_the_branch_local_day(h: &Harness) {
    let ani = h.pin_token(&h.jkt_device, &h.ani, ANI_PIN).await;
    let at = (ani.as_str(), h.jkt_device.as_str());

    let first = open(h, at, 0).await;
    assert_eq!(first.body["number"], "JKT01-R1-S-270115-01");
    let current = h
        .at_register(
            at,
            Method::GET,
            "/api/v1/cashier-sessions/current",
            &[],
            None,
        )
        .await;
    assert_eq!(current.body["number"], "JKT01-R1-S-270115-01");
    let report = close(h, at, &id_of(&first), 0).await;
    assert_eq!(report.body["session"]["number"], "JKT01-R1-S-270115-01");

    // 16:00Z = 23:00 WIB: still the 15th locally. (The idle PIN session expired meanwhile.)
    h.clock.advance(8 * HOUR);
    let ani = h.pin_token(&h.jkt_device, &h.ani, ANI_PIN).await;
    let at = (ani.as_str(), h.jkt_device.as_str());
    let second = open(h, at, 0).await;
    assert_eq!(second.body["number"], "JKT01-R1-S-270115-02");
    close(h, at, &id_of(&second), 0).await;

    // The branch moves to WIT (UTC+9): 16:00Z is 01:00 on the 16th there; NN restarts.
    let owner = h.owner_token().await;
    let uri = format!("/api/v1/branches/{}", h.jkt.id);
    let mut branch = h.call(&owner, Method::GET, &uri, None).await.body;
    assert_eq!(branch["timezone"], "Asia/Jakarta");
    branch["timezone"] = json!("Asia/Jayapura");
    let r = h.call(&owner, Method::PUT, &uri, Some(branch)).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert_eq!(r.body["timezone"], "Asia/Jayapura");
    let third = open(h, at, 0).await;
    assert_eq!(third.body["number"], "JKT01-R1-S-270116-01");

    // The audit entry and the stored row carry the number too.
    let detail = h
        .at_register(at, Method::GET, &session_uri(&id_of(&third)), &[], None)
        .await;
    assert_eq!(detail.body["session"]["number"], "JKT01-R1-S-270116-01");
}

async fn branch_timezone_is_validated(h: &Harness) {
    let owner = h.owner_token().await;
    let id = crate::common::new_id();
    let uri = format!("/api/v1/branches/{id}");
    let body = |tz: Option<&str>| {
        let mut b = json!({ "code": "SBY01", "name": "Surabaya" });
        if let Some(tz) = tz {
            b["timezone"] = json!(tz);
        }
        b
    };
    #[rustfmt::skip]
    let cases = [
        (body(None), Ok("Asia/Jakarta")),
        (body(Some(" Asia/Makassar ")), Ok("Asia/Makassar")),
        (body(Some("WIB")), Err(())),
        (body(Some("+07:00")), Err(())),
        (body(Some("asia/jakarta")), Err(())),
        (body(Some("")), Err(())),
    ];
    for (body, expected) in cases {
        let r = h.call(&owner, Method::PUT, &uri, Some(body.clone())).await;
        if let Ok(tz) = expected {
            assert!(r.status.is_success(), "{body}: {}", r.body);
            assert_eq!(r.body["timezone"], tz, "{body}");
        } else {
            r.assert_problem(422, "invalid_field");
            assert_eq!(r.body["field"], "timezone", "{body}");
        }
    }
    let listed = h.call(&owner, Method::GET, "/api/v1/branches", None).await;
    let sby = listed
        .body
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["id"] == id);
    assert_eq!(sby.unwrap()["timezone"], "Asia/Makassar");
}

#[allow(clippy::too_many_lines)] // one flow: open, read as five callers, close, read again
async fn get_shows_the_session_and_its_stored_report(h: &Harness) {
    let app = || router(h.state(h.node_role).with_movements(Arc::new(Fixed)));
    let ani = h.pin_token(&h.jkt_device, &h.ani, ANI_PIN).await;
    let ani_headers = |token: &str| {
        [
            ("authorization".to_owned(), format!("Bearer {token}")),
            ("x-register-token".to_owned(), h.jkt_device.clone()),
        ]
    };
    let send = |token: String, method: Method, uri: String, body: Option<Value>| {
        let headers = ani_headers(&token);
        let app = app();
        async move {
            let headers: Vec<(&str, &str)> = headers
                .iter()
                .map(|(n, v)| (n.as_str(), v.as_str()))
                .collect();
            Harness::send_to(app, method, &uri, &headers, body).await
        }
    };
    let opened = send(
        ani.clone(),
        Method::POST,
        OPEN.to_owned(),
        Some(json!({ "opening_cash": 200_000 })),
    )
    .await;
    let id = id_of(&opened);
    let uri = session_uri(&id);

    let budi = h.manager_token().await;
    let viewer = h
        .add_user("vera", seed::VIEWER_ROLE, BranchScope::Branch(h.jkt.id))
        .await;
    let vera = h.password_token(&viewer.username, BUDI_PASSWORD).await;
    let cici = h.pin_token(&h.bdg_device, &h.cici, ANI_PIN).await;
    let cici_get = |uri: String| {
        let app = app();
        let bearer = format!("Bearer {cici}");
        let device = h.bdg_device.clone();
        async move {
            Harness::send_to(
                app,
                Method::GET,
                &uri,
                &[("authorization", &bearer), ("x-register-token", &device)],
                None,
            )
            .await
        }
    };
    let owner = h.owner_token().await;
    let back_office = |token: String, uri: String| {
        let app = app();
        async move {
            let bearer = format!("Bearer {token}");
            Harness::send_to(app, Method::GET, &uri, &[("authorization", &bearer)], None).await
        }
    };

    // While open: blind close. Only `session.view_expected` holders see the live expected cash.
    #[rustfmt::skip]
    let open_cases = [
        ("own cashier", send(ani.clone(), Method::GET, uri.clone(), None).await, Some(None)),
        ("manager at the branch", back_office(budi.clone(), uri.clone()).await, Some(Some(290_000))),
        ("viewer at the branch", back_office(vera.clone(), uri.clone()).await, Some(None)),
        ("owner", back_office(owner.clone(), uri.clone()).await, Some(Some(290_000))),
        ("cashier at another branch", cici_get(uri.clone()).await, None),
    ];
    for (who, r, expected) in open_cases {
        if let Some(expected_cash) = expected {
            assert_eq!(r.status, StatusCode::OK, "{who}: {}", r.body);
            assert_eq!(r.body["session"]["status"], "open", "{who}");
            assert_eq!(r.body["session"]["number"], opened.body["number"], "{who}");
            assert_eq!(r.body["close_report"], Value::Null, "{who}");
            assert_eq!(
                r.body["session"]
                    .get("expected_cash")
                    .and_then(Value::as_i64),
                expected_cash,
                "{who}"
            );
        } else {
            r.assert_problem(403, "permission_denied");
            assert_eq!(r.body["permission"], "report.sales", "{who}");
        }
    }

    // After the close: the stored report, exactly as the close returned it, for everyone allowed.
    let closed = send(
        ani.clone(),
        Method::POST,
        close_uri(&id),
        Some(json!({ "counted_cash": 289_000, "counts": [
            { "denomination": 100_000, "quantity": 2 },
            { "denomination": 1_000, "quantity": 89 },
        ] })),
    )
    .await;
    assert_eq!(closed.status, StatusCode::OK, "{}", closed.body);
    #[rustfmt::skip]
    assert_eq!(
        (
            &closed.body["branch_code"], &closed.body["branch_name"], &closed.body["timezone"],
            &closed.body["register_code"], &closed.body["register_name"],
            &closed.body["cashier_name"], &closed.body["forced_by_name"],
        ),
        (
            &json!("JKT01"), &json!("Toko JKT01"), &json!("Asia/Jakarta"), &json!("R1"),
            &json!("Front"), &json!("ANI"), &Value::Null,
        )
    );
    assert_eq!(closed.body["expected_cash"], 290_000);
    assert_eq!(closed.body["over_short"], -1_000);
    for (who, r) in [
        (
            "own cashier",
            send(ani.clone(), Method::GET, uri.clone(), None).await,
        ),
        ("viewer", back_office(vera.clone(), uri.clone()).await),
        ("manager", back_office(budi.clone(), uri.clone()).await),
    ] {
        assert_eq!(r.status, StatusCode::OK, "{who}: {}", r.body);
        assert_eq!(r.body["close_report"], closed.body, "{who}");
        assert_eq!(r.body["session"], closed.body["session"], "{who}");
        assert_eq!(r.body["session"]["expected_cash"], 290_000, "{who}");
    }
    cici_get(uri.clone())
        .await
        .assert_problem(403, "permission_denied");
    back_office(owner.clone(), session_uri(&crate::common::new_id()))
        .await
        .assert_problem(404, "not_found");
}

async fn list_filters_pages_and_visibility(h: &Harness) {
    let ani = h.pin_token(&h.jkt_device, &h.ani, ANI_PIN).await;
    let ani_at = (ani.as_str(), h.jkt_device.as_str());
    let a = open(h, ani_at, 0).await;
    let a_id = id_of(&a);
    close(h, ani_at, &a_id, 0).await;
    h.clock.advance(MINUTES_10);
    let r2 = h.second_jkt_register().await;
    let budi_pin = h.pin_token(&r2, &h.budi, BUDI_PIN).await;
    let b_id = id_of(&open(h, (&budi_pin, &r2), 0).await);
    h.clock.advance(MINUTES_10);
    let cici = h.pin_token(&h.bdg_device, &h.cici, ANI_PIN).await;
    let c_id = id_of(&open(h, (&cici, &h.bdg_device), 0).await);
    let t1 = h.now().unix_millis() - MINUTES_10;

    let owner = h.owner_token().await;
    let budi = h.manager_token().await;
    let viewer = h
        .add_user("vera", seed::VIEWER_ROLE, BranchScope::Branch(h.jkt.id))
        .await;
    let vera = h.password_token(&viewer.username, BUDI_PASSWORD).await;
    let ids = |r: &Response| -> Vec<String> {
        assert_eq!(r.status, StatusCode::OK, "{}", r.body);
        r.body["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["id"].as_str().unwrap().to_owned())
            .collect()
    };
    let q = |query: &str| format!("{LIST}?{query}");
    #[rustfmt::skip]
    let cases: Vec<(&str, &str, String, Vec<&String>)> = vec![
        // Visibility: Owner everything; Manager and Viewer at JKT01 its sessions; cashiers their own.
        ("owner", &owner, LIST.to_owned(), vec![&c_id, &b_id, &a_id]),
        ("manager", &budi, LIST.to_owned(), vec![&b_id, &a_id]),
        ("viewer", &vera, LIST.to_owned(), vec![&b_id, &a_id]),
        ("cashier", &cici, LIST.to_owned(), vec![&c_id]),
        ("cashier asking for others", &cici, q(&format!("branch_id={}", h.jkt.id)), vec![]),
        // Filters.
        ("status=open", &owner, q("status=open"), vec![&c_id, &b_id]),
        ("status=closed", &owner, q("status=closed"), vec![&a_id]),
        ("branch", &owner, q(&format!("branch_id={}", h.bdg.id)), vec![&c_id]),
        ("register", &owner, q(&format!("register_id={}", h.jkt_register.id)), vec![&a_id]),
        ("user", &owner, q(&format!("user_id={}", h.budi.id)), vec![&b_id]),
        ("from", &owner, q(&format!("from={t1}")), vec![&c_id, &b_id]),
        ("until", &owner, q(&format!("until={t1}")), vec![&a_id]),
        ("page 2", &owner, q(&format!("limit=2&before_id={b_id}")), vec![&a_id]),
    ];
    for (name, token, uri, expected) in cases {
        let r = if token == cici {
            h.at_register((token, &h.bdg_device), Method::GET, &uri, &[], None)
                .await
        } else {
            h.call(token, Method::GET, &uri, None).await
        };
        let expected: Vec<String> = expected.into_iter().cloned().collect();
        assert_eq!(ids(&r), expected, "{name}");
    }

    // Paging: `next_before_id` until the last page.
    let page = h.call(&owner, Method::GET, &q("limit=2"), None).await;
    assert_eq!(ids(&page), [c_id.clone(), b_id.clone()]);
    assert_eq!(page.body["next_before_id"], b_id.as_str());
    let last = h
        .call(
            &owner,
            Method::GET,
            &q(&format!("limit=2&before_id={b_id}")),
            None,
        )
        .await;
    assert_eq!(last.body["next_before_id"], Value::Null);

    // Blind close in the list: open sessions show expected cash only to `session.view_expected`.
    let items = |r: Response| r.body["items"].as_array().unwrap().clone();
    let viewer_items = items(h.call(&vera, Method::GET, LIST, None).await);
    assert!(
        viewer_items[0].get("expected_cash").is_none(),
        "{viewer_items:?}"
    );
    assert_eq!(
        viewer_items[1]["expected_cash"], 0,
        "closed: the figure of the close"
    );
    let manager_items = items(h.call(&budi, Method::GET, LIST, None).await);
    assert_eq!(manager_items[0]["expected_cash"], 0);
    assert_eq!(manager_items[0]["number"], "JKT01-R2-S-270115-01");

    // A register PIN session may list too (its own sessions).
    let r = h.at_register(ani_at, Method::GET, LIST, &[], None).await;
    assert_eq!(ids(&r), [a_id]);
}

async fn list_and_get_reject_bad_input(h: &Harness) {
    let owner = h.owner_token().await;
    #[rustfmt::skip]
    let cases = [
        (format!("{LIST}?status=paused"), 422, "invalid_field"),
        (format!("{LIST}?limit=0"), 422, "invalid_field"),
        (format!("{LIST}?limit=501"), 422, "invalid_field"),
        (format!("{LIST}?branch_id=nope"), 400, "invalid_body"),
        (format!("{LIST}?before_id=nope"), 400, "invalid_body"),
        (format!("{LIST}?from=yesterday"), 400, "invalid_query"),
        (session_uri("nope"), 400, "invalid_body"),
    ];
    for (uri, status, code) in cases {
        h.call(&owner, Method::GET, &uri, None)
            .await
            .assert_problem(status, code);
    }
    h.send(Method::GET, LIST, &[], None)
        .await
        .assert_problem(401, "invalid_session");
}

/// Concurrent opens on one register: exactly one `201`; the others `409 session_already_open`
/// (the server reruns a transaction that lost the race), never a `500`.
async fn racing_opens_get_one_session(h: &Harness) {
    race(h).await;
}

async fn race(h: &Harness) {
    let mut tokens = Vec::new();
    for n in 0..6 {
        let u = h
            .add_user(
                &format!("racer{n}"),
                seed::CASHIER_ROLE,
                BranchScope::Branch(h.jkt.id),
            )
            .await;
        tokens.push(h.pin_token(&h.jkt_device, &u, ANI_PIN).await);
    }
    let tasks: Vec<_> = tokens
        .into_iter()
        .map(|token| {
            let app = h.app();
            let device = h.jkt_device.clone();
            tokio::spawn(async move {
                let bearer = format!("Bearer {token}");
                Harness::send_to(
                    app,
                    Method::POST,
                    OPEN,
                    &[("authorization", &bearer), ("x-register-token", &device)],
                    Some(json!({ "opening_cash": 0 })),
                )
                .await
            })
        })
        .collect();
    let mut statuses = Vec::new();
    for task in tasks {
        let r = task.await.unwrap();
        if r.status != StatusCode::CREATED {
            r.assert_problem(409, "session_already_open");
        }
        statuses.push(r.status.as_u16());
    }
    statuses.sort_unstable();
    assert_eq!(statuses, [201, 409, 409, 409, 409, 409]);
    let numbers = h
        .call(&h.owner_token().await, Method::GET, LIST, None)
        .await;
    assert_eq!(numbers.body["items"][0]["number"], "JKT01-R1-S-270115-01");
}

/// The same race on a `SQLite` file with a real connection pool, where losers see
/// `SQLITE_BUSY_SNAPSHOT` (multi-threaded runtime).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn racing_opens_on_a_sqlite_file() {
    let dir = std::env::temp_dir().join(format!("sultan-race-{}", uuid::Uuid::now_v7()));
    std::fs::create_dir_all(&dir).unwrap();
    let db = Db::connect(&format!("sqlite://{}", dir.join("race.db").display()))
        .await
        .unwrap();
    let h = Harness::with_db(db).await;
    race(&h).await;
    h.db.close().await;
    std::fs::remove_dir_all(dir).unwrap();
}
