//! [`HttpService`] against the real router on `127.0.0.1:0` (plan Phase 8), the way the app tests
//! run `sultan serve`: login, wrong PIN and lockout (spec 2.1 §4), open / `409` / close (§5),
//! the override flow (§6), the events socket and the error mapping.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use sultan_client::{
    ClientError, CloseRequest, Count, EventSink, HttpService, OverrideRequest, ProtectedAction,
    ServerEvent, Service, SessionQuery,
};
use sultan_core::access::BranchScope;
use sultan_core::ids::{BranchId, CompanyId, RegisterId, UserId};
use sultan_db::org::{Branch, Company, Register};
use sultan_db::users::User;
use sultan_db::{Db, devices, org, roles, seed, users};
use sultan_server::{AppState, Clock, NodeRole, SystemClock, credentials, router, token};

const ANI_PIN: &str = "1234";
const BUDI_PIN: &str = "9999";

/// A served organization: branch JKT01 with paired register R1, cashier Ani and manager Budi.
struct Server {
    root: String,
    device: String,
    ani: String,
    budi: String,
    branch: String,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn user(name: &str, pin: &str) -> User {
    User {
        id: UserId::generate(),
        username: name.to_lowercase(),
        display_name: name.to_owned(),
        pin_hash: Some(credentials::hash_pin(pin).unwrap()),
        password_hash: None,
        active: true,
    }
}

async fn serve() -> Server {
    let db = Db::connect("sqlite::memory:").await.unwrap();
    db.migrate().await.unwrap();
    let now = SystemClock.now();
    seed::seed_defaults(&db, now).await.unwrap();
    let company = Company {
        id: CompanyId::generate(),
        name: "PT Sultan".to_owned(),
    };
    let branch = Branch {
        id: BranchId::generate(),
        company: company.id,
        code: "JKT01".to_owned(),
        name: "Toko Jakarta".to_owned(),
        address: String::new(),
        npwp: None,
        receipt_header: String::new(),
        receipt_footer: String::new(),
        timezone: "Asia/Jakarta".to_owned(),
        archived: false,
    };
    let register = Register {
        id: RegisterId::generate(),
        branch: branch.id,
        code: "R1".to_owned(),
        name: "Kasir 1".to_owned(),
        default_location: None,
        archived: false,
    };
    let ani = user("Ani", ANI_PIN);
    let budi = user("Budi", BUDI_PIN);
    let device = token::generate();
    let mut tx = db.begin().await.unwrap();
    org::save_company(&mut tx, &company, now).await.unwrap();
    org::save_branch(&mut tx, &branch, now).await.unwrap();
    org::save_register(&mut tx, &register, now).await.unwrap();
    for (u, role) in [(&ani, seed::CASHIER_ROLE), (&budi, seed::MANAGER_ROLE)] {
        users::save_user(&mut tx, u, now).await.unwrap();
        roles::replace_assignments(
            &mut tx,
            u.id,
            &[(role, BranchScope::Branch(branch.id))],
            now,
        )
        .await
        .unwrap();
    }
    devices::insert_device_token(&mut tx, &token::hash(&device), register.id, now)
        .await
        .unwrap();
    tx.commit().await.unwrap();

    let state = AppState::new(db, NodeRole::Standalone, Arc::new(SystemClock));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        axum::serve(listener, router(state)).await.unwrap();
    });
    Server {
        root: format!("http://{addr}"),
        device,
        ani: ani.id.to_string(),
        budi: budi.id.to_string(),
        branch: branch.id.to_string(),
        task,
    }
}

impl Server {
    fn client(&self) -> HttpService {
        HttpService::new(&self.root, &self.device)
    }
}

#[track_caller]
fn assert_code<T: std::fmt::Debug>(
    result: Result<T, ClientError>,
    status: u16,
    code: &str,
) -> sultan_client::Problem {
    let err = result.unwrap_err();
    let problem = err.problem().unwrap_or_else(|| panic!("{err:?}")).clone();
    assert_eq!(
        (problem.status, problem.code.as_str()),
        (status, code),
        "{problem:?}"
    );
    problem
}

#[tokio::test]
async fn login_wrong_pin_and_lockout() {
    let s = serve().await;
    let c = s.client();
    let users = c.lock_users().await.unwrap();
    let names: Vec<_> = users.iter().map(|u| u.display_name.as_str()).collect();
    assert_eq!(names.len(), 2);
    assert!(
        names.contains(&"Ani") && names.contains(&"Budi"),
        "{names:?}"
    );

    let login = c.pin_login(&s.ani, ANI_PIN).await.unwrap();
    assert_eq!(login.display_name, "Ani");
    assert_eq!(login.branch_id, s.branch);
    assert!(login.register_id.is_some());
    let branch = c.branch(&login.token, &s.branch).await.unwrap();
    assert_eq!(
        (branch.code.as_str(), branch.name.as_str()),
        ("JKT01", "Toko Jakarta")
    );

    for _ in 0..4 {
        assert_code(
            c.pin_login(&s.ani, "0000").await,
            401,
            "invalid_credentials",
        );
    }
    let locked = assert_code(c.pin_login(&s.ani, "0000").await, 423, "locked_out");
    let until = locked.locked_until().unwrap();
    assert!(until > SystemClock.now().unix_millis(), "{until}");
    // The right PIN does not help while locked.
    let again = assert_code(c.pin_login(&s.ani, ANI_PIN).await, 423, "locked_out");
    assert_eq!(again.locked_until(), Some(until));

    c.logout(&login.token).await.unwrap();
    assert_code(
        c.current_session(&login.token).await,
        401,
        "invalid_session",
    );
}

#[tokio::test]
async fn open_conflict_and_blind_close() {
    let s = serve().await;
    let c = s.client();
    let ani = c.pin_login(&s.ani, ANI_PIN).await.unwrap().token;
    let budi = c.pin_login(&s.budi, BUDI_PIN).await.unwrap().token;
    assert_eq!(c.current_session(&ani).await.unwrap(), None);

    let opened = c.open_session(&ani, 500_000, "open-1").await.unwrap();
    assert_eq!(
        (opened.opening_cash, opened.status.as_str()),
        (500_000, "open")
    );
    assert_eq!(
        opened.expected_cash, None,
        "a cashier never sees expected cash (blind close)"
    );
    // The same key is a retry: same answer, no 409.
    let replay = c.open_session(&ani, 500_000, "open-1").await.unwrap();
    assert_eq!(replay.id, opened.id);
    // Another user on the same register: 409 with the conflict.
    let conflict = assert_code(
        c.open_session(&budi, 1, "open-2").await,
        409,
        "session_already_open",
    );
    assert_eq!(conflict.conflict(), Some("register"));
    assert_eq!(conflict.session_id(), Some(opened.id.as_str()));
    // Anyone at the register sees whose session is open; the manager also sees expected cash.
    let current = c.current_session(&budi).await.unwrap().unwrap();
    assert_eq!(
        (current.id.as_str(), current.user_id.as_str()),
        (opened.id.as_str(), s.ani.as_str())
    );
    assert_eq!(current.expected_cash, Some(500_000));

    let body = CloseRequest {
        counted_cash: 510_000,
        counts: Some(vec![
            Count {
                denomination: 100_000,
                quantity: 5,
            },
            Count {
                denomination: 10_000,
                quantity: 1,
            },
        ]),
    };
    assert_code(
        c.close_session(&budi, &opened.id, &body, "close-0").await,
        403,
        "permission_denied",
    );
    let report = c
        .close_session(&ani, &opened.id, &body, "close-1")
        .await
        .unwrap();
    assert_eq!(
        (report.expected_cash, report.counted_cash, report.over_short),
        (500_000, 510_000, 10_000)
    );
    assert_eq!(report.counts.len(), 2);
    assert!(!report.forced && report.tenders.is_empty());
    assert_eq!(report.session.status, "closed");
    assert_code(
        c.close_session(&ani, &opened.id, &body, "close-2").await,
        409,
        "session_closed",
    );
    assert_eq!(c.current_session(&ani).await.unwrap(), None);

    // Counts that do not add up are a field error.
    let reopened = c.open_session(&ani, 0, "open-3").await.unwrap();
    let bad = CloseRequest {
        counted_cash: 1,
        counts: Some(vec![Count {
            denomination: 2,
            quantity: 1,
        }]),
    };
    let problem = assert_code(
        c.close_session(&ani, &reopened.id, &bad, "close-3").await,
        422,
        "invalid_field",
    );
    assert_eq!(problem.field(), Some("counts"));
}

/// Phase 9: session numbers, `GET /cashier-sessions/{id}` and the list.
#[tokio::test]
async fn past_sessions_and_reports() {
    let s = serve().await;
    let c = s.client();
    let ani = c.pin_login(&s.ani, ANI_PIN).await.unwrap().token;
    let budi = c.pin_login(&s.budi, BUDI_PIN).await.unwrap().token;
    let opened = c.open_session(&ani, 100_000, "open-a").await.unwrap();
    assert!(
        opened.number.starts_with("JKT01-R1-S-") && opened.number.ends_with("-01"),
        "{}",
        opened.number
    );
    let detail = c.session(&ani, &opened.id).await.unwrap();
    assert_eq!(
        (detail.session.number, detail.close_report),
        (opened.number.clone(), None)
    );
    let body = CloseRequest {
        counted_cash: 99_000,
        counts: None,
    };
    let report = c
        .close_session(&ani, &opened.id, &body, "close-a")
        .await
        .unwrap();
    assert_eq!(
        (report.branch_code.as_str(), report.register_code.as_str()),
        ("JKT01", "R1")
    );
    assert_eq!(report.cashier_name, "Ani");
    assert!(report.session.closed_at.is_some());
    // The stored report is the close response (the source of a reprint).
    let detail = c.session(&budi, &opened.id).await.unwrap();
    assert_eq!(detail.close_report, Some(report.clone()));

    let second = c.open_session(&budi, 0, "open-b").await.unwrap();
    assert!(second.number.ends_with("-02"), "{}", second.number);
    let page = c
        .sessions(
            &budi,
            &SessionQuery {
                limit: Some(1),
                ..SessionQuery::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].id, second.id);
    assert_eq!(page.next_before_id.as_deref(), Some(second.id.as_str()));
    let page = c
        .sessions(
            &ani,
            &SessionQuery {
                status: Some("closed".into()),
                ..SessionQuery::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(page.items, vec![report.session]);
    // The cashier may not read the manager's session.
    assert_code(c.session(&ani, &second.id).await, 403, "permission_denied");
    assert_code(
        c.sessions(
            &ani,
            &SessionQuery {
                status: Some("paused".into()),
                ..SessionQuery::default()
            },
        )
        .await,
        422,
        "invalid_field",
    );
}

#[tokio::test]
async fn override_flow() {
    let s = serve().await;
    let c = s.client();
    let ani = c.pin_login(&s.ani, ANI_PIN).await.unwrap().token;
    let action = ProtectedAction {
        action: "sale.discount".to_owned(),
        value: Some(1_500),
        context_ref: "cart-1".to_owned(),
    };
    let needs = assert_code(
        c.demo_protected(&ani, &action, None).await,
        403,
        "needs_override",
    );
    assert_eq!(
        (needs.permission(), needs.limit()),
        (Some("sale.discount"), Some(1_000))
    );

    let request = |approver: &str, pin: &str, value: i64| OverrideRequest {
        action: action.action.clone(),
        value: Some(value),
        context_ref: action.context_ref.clone(),
        approver_id: approver.to_owned(),
        approver_pin: pin.to_owned(),
    };
    assert_code(
        c.request_override(&ani, &request(&s.ani, ANI_PIN, 1_500))
            .await,
        403,
        "self_approval",
    );
    assert_code(
        c.request_override(&ani, &request(&s.budi, "0000", 1_500))
            .await,
        401,
        "invalid_credentials",
    );
    let limit = assert_code(
        c.request_override(&ani, &request(&s.budi, BUDI_PIN, 2_500))
            .await,
        403,
        "override_limit_exceeded",
    );
    assert_eq!(limit.limit(), Some(2_000));

    let grant = c
        .request_override(&ani, &request(&s.budi, BUDI_PIN, 1_500))
        .await
        .unwrap();
    assert_eq!(
        (grant.approver_id.as_str(), grant.requester_id.as_str()),
        (s.budi.as_str(), s.ani.as_str())
    );
    let done = c
        .demo_protected(&ani, &action, Some(&grant.token))
        .await
        .unwrap();
    assert_eq!(done.approved_by.as_deref(), Some(s.budi.as_str()));
    // Single use.
    assert_code(
        c.demo_protected(&ani, &action, Some(&grant.token)).await,
        403,
        "invalid_override",
    );
    // Within the cashier's own limit: no override needed.
    let small = ProtectedAction {
        value: Some(500),
        ..action
    };
    assert_eq!(
        c.demo_protected(&ani, &small, None)
            .await
            .unwrap()
            .approved_by,
        None
    );
}

#[tokio::test]
async fn events_report_the_logout() {
    let s = serve().await;
    let c = s.client();
    let ani = c.pin_login(&s.ani, ANI_PIN).await.unwrap().token;
    let seen: Arc<Mutex<Vec<ServerEvent>>> = Arc::default();
    let sink: EventSink = {
        let seen = seen.clone();
        Arc::new(move |e| seen.lock().unwrap().push(e))
    };
    let watcher = tokio::spawn(c.watch_events(&ani, sink));
    // Each open and close is broadcast to the branch; the first may come before the socket has
    // subscribed, so toggle until one arrives.
    let zero = CloseRequest {
        counted_cash: 0,
        counts: None,
    };
    let mut subscribed = false;
    for attempt in 0..50 {
        match c.current_session(&ani).await.unwrap() {
            None => drop(
                c.open_session(&ani, 0, &format!("open-{attempt}"))
                    .await
                    .unwrap(),
            ),
            Some(open) => drop(
                c.close_session(&ani, &open.id, &zero, &format!("close-{attempt}"))
                    .await
                    .unwrap(),
            ),
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
        if !seen.lock().unwrap().is_empty() {
            subscribed = true;
            break;
        }
    }
    assert!(subscribed);
    c.logout(&ani).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), watcher)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let last = seen.lock().unwrap().last().cloned().unwrap();
    assert!(last.revokes_this_session(), "{last:?}");

    // A revoked session cannot open a socket.
    let err = c.watch_events(&ani, Arc::new(|_| {})).await.unwrap_err();
    assert!(matches!(err, ClientError::Network(_)), "{err:?}");
}

#[tokio::test]
async fn errors_are_mapped() {
    let s = serve().await;
    let unpaired = HttpService::new(&s.root, "not-a-device");
    assert_code(unpaired.lock_users().await, 401, "invalid_device_token");
    let c = s.client();
    assert_code(c.pin_login("nope", "1234").await, 400, "invalid_body");
    assert_code(c.open_session("nope", 1, "k").await, 401, "invalid_session");

    // Nothing listens: a network error.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let closed = listener.local_addr().unwrap();
    drop(listener);
    let gone = HttpService::new(&format!("http://{closed}"), "d");
    assert!(matches!(
        gone.lock_users().await,
        Err(ClientError::Network(_))
    ));
    assert!(matches!(
        gone.logout("t").await,
        Err(ClientError::Network(_))
    ));

    // Answers that are not problem+json, or not the expected body.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let odd = axum::Router::new()
        .route(
            "/api/v1/registers/me/users",
            axum::routing::get(async || "hello"),
        )
        .route(
            "/api/v1/cashier-sessions/current",
            axum::routing::get(async || (axum::http::StatusCode::BAD_GATEWAY, "oops")),
        );
    let task = tokio::spawn(async move { axum::serve(listener, odd).await.unwrap() });
    let weird = HttpService::new(&format!("http://{addr}"), "d");
    let err = weird.lock_users().await.unwrap_err();
    assert!(
        matches!(&err, ClientError::Unexpected(m) if m.starts_with("HTTP 200")),
        "{err:?}"
    );
    let err = weird.current_session("t").await.unwrap_err();
    assert_eq!(err, ClientError::Unexpected("HTTP 502 Bad Gateway".into()));
    // A token that cannot be a header value.
    let err = weird
        .watch_events("bad\ntoken", Arc::new(|_| {}))
        .await
        .unwrap_err();
    assert!(matches!(err, ClientError::Unexpected(_)), "{err:?}");
    task.abort();
}
