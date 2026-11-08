//! Test harness: a seeded organization behind the real router, driven in-process with
//! `tower::ServiceExt::oneshot`. Every test runs on in-memory `SQLite`, and again on a fresh
//! `MySQL` database when `SULTAN_TEST_MYSQL_URL` is set (as in `crates/db/tests`).

use std::str::FromStr;
use std::sync::{Arc, OnceLock};

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Method, Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::Value;
use sqlx::mysql::{MySqlConnectOptions, MySqlPool, MySqlPoolOptions};
use sultan_core::access::BranchScope;
use sultan_core::ids::{BranchId, CompanyId, RegisterId, RoleId, UserId};
use sultan_core::time::Timestamp;
use sultan_db::org::{Branch, Company, Register};
use sultan_db::users::User;
use sultan_db::{Db, devices, org, roles, seed, users};
use sultan_server::events::EventHub;
use sultan_server::{AppState, Clock, ManualClock, NodeRole, credentials, router, token};
use tower::ServiceExt;

/// Generates `sqlite::<name>` and `mysql::<name>` tests for each `async fn <name>(h: &Harness)`.
macro_rules! api_tests {
    ($($name:ident),* $(,)?) => {
        mod sqlite {
            $(
                #[tokio::test]
                async fn $name() {
                    let h = crate::common::Harness::sqlite().await;
                    super::$name(&h).await;
                }
            )*
        }
        mod mysql {
            $(
                #[tokio::test]
                async fn $name() {
                    if let Some(h) = crate::common::Harness::mysql().await {
                        super::$name(&h).await;
                        h.drop_mysql().await;
                    }
                }
            )*
        }
    };
}
pub(crate) use api_tests;

/// Start of the test clock.
pub const T0: Timestamp = Timestamp::from_unix_millis(1_800_000_000_000);
/// Session idle expiry used by the harness: 30 minutes.
pub const IDLE_MS: i64 = 30 * 60 * 1_000;

pub const ANI_PIN: &str = "1234";
pub const BUDI_PIN: &str = "9999";
pub const BUDI_PASSWORD: &str = "manager-pass";
pub const OWNER_PASSWORD: &str = "owner-pass-1";

/// argon2 hashes are slow; compute each once per test binary.
struct Hashes {
    ani_pin: String,
    budi_pin: String,
    budi_password: String,
    owner_password: String,
}

fn hashes() -> &'static Hashes {
    static HASHES: OnceLock<Hashes> = OnceLock::new();
    HASHES.get_or_init(|| Hashes {
        ani_pin: credentials::hash_pin(ANI_PIN).unwrap(),
        budi_pin: credentials::hash_pin(BUDI_PIN).unwrap(),
        budi_password: credentials::hash_password(BUDI_PASSWORD).unwrap(),
        owner_password: credentials::hash_password(OWNER_PASSWORD).unwrap(),
    })
}

/// The organization:
/// - branches JKT01 and BDG01, register R1 at each, both paired (device tokens below);
/// - `ani`: Cashier at JKT01, PIN only;
/// - `budi`: Manager at JKT01, PIN and password;
/// - `cici`: Cashier at BDG01 only, same PIN as `ani`;
/// - `owner`: Owner at all branches, password only.
pub struct Harness {
    pub db: Db,
    pub clock: ManualClock,
    pub node_role: NodeRole,
    pub jkt: Branch,
    pub bdg: Branch,
    pub jkt_register: Register,
    pub bdg_register: Register,
    pub jkt_device: String,
    pub bdg_device: String,
    pub ani: User,
    pub budi: User,
    pub cici: User,
    pub owner: User,
    /// Shared by every app this harness builds, so event sockets see events of other requests.
    pub events: EventHub,
    mysql: Option<(MySqlPool, String)>,
}

pub struct Response {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Value,
}

impl Response {
    /// Asserts a problem+json error with this status and code.
    #[track_caller]
    pub fn assert_problem(&self, status: u16, code: &str) {
        assert_eq!(
            (self.status.as_u16(), self.body["code"].as_str()),
            (status, Some(code)),
            "{}",
            self.body
        );
        assert_eq!(self.headers["content-type"], "application/problem+json");
        assert_eq!(self.body["status"], status);
    }

    /// The `token` field of a login response.
    pub fn token(&self) -> String {
        self.body["token"].as_str().unwrap().to_owned()
    }
}

fn user(username: &str, pin: Option<&str>, password: Option<&str>) -> User {
    User {
        id: UserId::generate(),
        username: username.to_owned(),
        display_name: username.to_uppercase(),
        pin_hash: pin.map(str::to_owned),
        password_hash: password.map(str::to_owned),
        active: true,
    }
}

fn branch(company: CompanyId, code: &str) -> Branch {
    Branch {
        id: BranchId::generate(),
        company,
        code: code.to_owned(),
        name: format!("Toko {code}"),
        address: String::new(),
        npwp: None,
        receipt_header: String::new(),
        receipt_footer: String::new(),
        timezone: "Asia/Jakarta".to_owned(),
        archived: false,
    }
}

fn register(branch: BranchId) -> Register {
    Register {
        id: RegisterId::generate(),
        branch,
        code: "R1".to_owned(),
        name: "Front".to_owned(),
        default_location: None,
        archived: false,
    }
}

/// A fresh client-generated ID, as text.
pub fn new_id() -> String {
    uuid::Uuid::now_v7().to_string()
}

/// A user that is not in the database.
pub fn stranger() -> User {
    user("stranger", None, None)
}

impl Harness {
    pub async fn sqlite() -> Self {
        let db = Db::connect("sqlite::memory:").await.unwrap();
        Self::build(db, None).await
    }

    /// The organization on `db` (e.g. a `SQLite` file with a real pool).
    pub async fn with_db(db: Db) -> Self {
        Self::build(db, None).await
    }

    /// `None` when `SULTAN_TEST_MYSQL_URL` is not set.
    pub async fn mysql() -> Option<Self> {
        let url = std::env::var("SULTAN_TEST_MYSQL_URL").ok()?;
        let admin = MySqlPoolOptions::new()
            .max_connections(1)
            .connect(&url)
            .await
            .unwrap();
        let name = format!("sultan_s_{}", uuid::Uuid::now_v7().simple());
        sqlx::query(&format!("CREATE DATABASE {name} CHARACTER SET utf8mb4"))
            .execute(&admin)
            .await
            .unwrap();
        let options = MySqlConnectOptions::from_str(&url).unwrap().database(&name);
        let pool = MySqlPoolOptions::new()
            .max_connections(4)
            .connect_with(options)
            .await
            .unwrap();
        Some(Self::build(Db::MySql(pool), Some((admin, name))).await)
    }

    pub async fn drop_mysql(self) {
        self.db.close().await;
        if let Some((admin, name)) = self.mysql {
            sqlx::query(&format!("DROP DATABASE {name}"))
                .execute(&admin)
                .await
                .unwrap();
        }
    }

    async fn build(db: Db, mysql: Option<(MySqlPool, String)>) -> Self {
        db.migrate().await.unwrap();
        seed::seed_defaults(&db, T0).await.unwrap();
        let h = hashes();
        let company = Company {
            id: CompanyId::generate(),
            name: "PT Sultan".to_owned(),
        };
        let jkt = branch(company.id, "JKT01");
        let bdg = branch(company.id, "BDG01");
        let jkt_register = register(jkt.id);
        let bdg_register = register(bdg.id);
        let ani = user("ani", Some(&h.ani_pin), None);
        let budi = user("budi", Some(&h.budi_pin), Some(&h.budi_password));
        let cici = user("cici", Some(&h.ani_pin), None);
        let owner = user("owner", None, Some(&h.owner_password));

        let mut tx = db.begin().await.unwrap();
        org::save_company(&mut tx, &company, T0).await.unwrap();
        for b in [&jkt, &bdg] {
            org::save_branch(&mut tx, b, T0).await.unwrap();
        }
        for r in [&jkt_register, &bdg_register] {
            org::save_register(&mut tx, r, T0).await.unwrap();
        }
        let assignments: [(&User, RoleId, BranchScope); 4] = [
            (&ani, seed::CASHIER_ROLE, BranchScope::Branch(jkt.id)),
            (&budi, seed::MANAGER_ROLE, BranchScope::Branch(jkt.id)),
            (&cici, seed::CASHIER_ROLE, BranchScope::Branch(bdg.id)),
            (&owner, seed::OWNER_ROLE, BranchScope::All),
        ];
        for (u, role, scope) in assignments {
            users::save_user(&mut tx, u, T0).await.unwrap();
            roles::replace_assignments(&mut tx, u.id, &[(role, scope)], T0)
                .await
                .unwrap();
        }
        let jkt_device = token::generate();
        let bdg_device = token::generate();
        for (t, r) in [(&jkt_device, &jkt_register), (&bdg_device, &bdg_register)] {
            devices::insert_device_token(&mut tx, &token::hash(t), r.id, T0)
                .await
                .unwrap();
        }
        tx.commit().await.unwrap();

        Self {
            db,
            clock: ManualClock::new(T0),
            node_role: NodeRole::Standalone,
            jkt,
            bdg,
            jkt_register,
            bdg_register,
            jkt_device,
            bdg_device,
            ani,
            budi,
            cici,
            owner,
            events: EventHub::default(),
            mysql,
        }
    }

    pub fn state(&self, node_role: NodeRole) -> AppState {
        AppState::new(self.db.clone(), node_role, Arc::new(self.clock.clone()))
            .with_session_idle_ms(IDLE_MS)
            .with_events(self.events.clone())
    }

    pub fn app(&self) -> Router {
        router(self.state(self.node_role))
    }

    /// Sends one request to `app`. `headers` are (name, value) pairs; `body` is sent as JSON.
    pub async fn send_to(
        app: Router,
        method: Method,
        uri: &str,
        headers: &[(&str, &str)],
        body: Option<Value>,
    ) -> Response {
        let mut request = Request::builder().method(method).uri(uri);
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        let request = match body {
            Some(json) => request
                .header("content-type", "application/json")
                .body(Body::from(json.to_string())),
            None => request.body(Body::empty()),
        }
        .unwrap();
        let response = app.oneshot(request).await.unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let body = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap()
        };
        Response {
            status,
            headers,
            body,
        }
    }

    pub async fn send(
        &self,
        method: Method,
        uri: &str,
        headers: &[(&str, &str)],
        body: Option<Value>,
    ) -> Response {
        Self::send_to(self.app(), method, uri, headers, body).await
    }

    /// `POST /auth/pin` from `device`.
    pub async fn pin_login(&self, device: &str, user: &User, pin: &str) -> Response {
        self.send(
            Method::POST,
            "/api/v1/auth/pin",
            &[("x-register-token", device)],
            Some(serde_json::json!({ "user_id": user.id.to_string(), "pin": pin })),
        )
        .await
    }

    /// `POST /auth/logout` with a bearer token and optional device token.
    pub async fn logout(&self, token: &str, device: Option<&str>) -> Response {
        let bearer = format!("Bearer {token}");
        let mut headers = vec![("authorization", bearer.as_str())];
        if let Some(d) = device {
            headers.push(("x-register-token", d));
        }
        self.send(Method::POST, "/api/v1/auth/logout", &headers, None)
            .await
    }

    /// `POST /users/{target}/unlock` as the holder of `token` on `device`.
    pub async fn unlock(&self, token: &str, device: &str, target: &str) -> Response {
        let bearer = format!("Bearer {token}");
        self.send(
            Method::POST,
            &format!("/api/v1/users/{target}/unlock"),
            &[("authorization", &bearer), ("x-register-token", device)],
            None,
        )
        .await
    }

    /// A back-office session token from `POST /auth/password`.
    pub async fn password_token(&self, username: &str, password: &str) -> String {
        let r = self
            .send(
                Method::POST,
                "/api/v1/auth/password",
                &[],
                Some(serde_json::json!({ "username": username, "password": password })),
            )
            .await;
        assert_eq!(r.status, StatusCode::OK, "{}", r.body);
        r.token()
    }

    /// A session of `owner` (Owner at all branches).
    pub async fn owner_token(&self) -> String {
        self.password_token("owner", OWNER_PASSWORD).await
    }

    /// A session of `budi` (Manager at JKT01: no `org.manage`, `user.manage` or `role.manage`).
    pub async fn manager_token(&self) -> String {
        self.password_token("budi", BUDI_PASSWORD).await
    }

    /// Sends a request with `Authorization: Bearer <token>`.
    pub async fn call(
        &self,
        token: &str,
        method: Method,
        uri: &str,
        body: Option<Value>,
    ) -> Response {
        let bearer = format!("Bearer {token}");
        self.send(method, uri, &[("authorization", &bearer)], body)
            .await
    }

    /// A PIN session token of `user` at the register of `device`.
    pub async fn pin_token(&self, device: &str, user: &User, pin: &str) -> String {
        let r = self.pin_login(device, user, pin).await;
        assert_eq!(r.status, StatusCode::OK, "{}", r.body);
        r.token()
    }

    /// Sends a request as a register session: bearer `token`, `X-Register-Token: device` and any
    /// `extra` headers.
    pub async fn at_register(
        &self,
        (token, device): (&str, &str),
        method: Method,
        uri: &str,
        extra: &[(&str, &str)],
        body: Option<Value>,
    ) -> Response {
        let bearer = format!("Bearer {token}");
        let mut headers = vec![
            ("authorization", bearer.as_str()),
            ("x-register-token", device),
        ];
        headers.extend_from_slice(extra);
        self.send(method, uri, &headers, body).await
    }

    /// Adds a second paired register `R2` at JKT01; returns its device token.
    pub async fn second_jkt_register(&self) -> String {
        let mut r2 = register(self.jkt.id);
        "R2".clone_into(&mut r2.code);
        let device = token::generate();
        let mut tx = self.db.begin().await.unwrap();
        org::save_register(&mut tx, &r2, T0).await.unwrap();
        devices::insert_device_token(&mut tx, &token::hash(&device), r2.id, T0)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        device
    }

    /// Adds an active user with a PIN (`ANI_PIN`) and password (`BUDI_PASSWORD`) holding `role`
    /// at `scope`.
    pub async fn add_user(&self, username: &str, role: RoleId, scope: BranchScope) -> User {
        let h = hashes();
        let u = user(username, Some(&h.ani_pin), Some(&h.budi_password));
        let mut tx = self.db.begin().await.unwrap();
        users::save_user(&mut tx, &u, T0).await.unwrap();
        roles::replace_assignments(&mut tx, u.id, &[(role, scope)], T0)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        u
    }

    /// How many outbox events exist.
    pub async fn outbox_count(&self) -> usize {
        sultan_db::outbox::list_unsynced(&self.db, u32::MAX)
            .await
            .unwrap()
            .len()
    }

    /// The test clock's current time.
    pub fn now(&self) -> Timestamp {
        self.clock.now()
    }

    /// How many audit entries with `action` exist.
    pub async fn audit_count(&self, action: &str) -> usize {
        let filter = sultan_db::audit::AuditFilter {
            action: Some(action.to_owned()),
            ..Default::default()
        };
        sultan_db::audit::query(&self.db, &filter)
            .await
            .unwrap()
            .len()
    }
}
