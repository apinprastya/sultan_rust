//! Cash management (phase K3, spec 2.7): every cash call of [`HttpService`] against the real
//! router (in-memory `SQLite`), and the same flow against [`FakeService`], so the register and
//! back-office screens of K4 and K5 see the server's answers: cash reasons, cash in / out / drop
//! with overrides, replays and reversals, slips, the X report, the close with the float kept,
//! the close report text, the proposed float, the Z with its text and the days still to close,
//! and the back office's cash reasons, Z list and force close.

use std::sync::Arc;

use sultan_client::fake::{FakeService, SEED_REASONS};
use sultan_client::{
    BackOffice, CashMovementRequest, CashQuery, CashReasonBody, ClientError, CloseRequest,
    HttpService, OverrideRequest, Service, ZQuery, ZRunRequest,
};
use sultan_core::access::BranchScope;
use sultan_core::doc_number::{BranchTimeZone, LocalDay};
use sultan_core::ids::{BranchId, CompanyId, ProductId, RegisterId, UserId};
use sultan_db::org::{Branch, Company, Register};
use sultan_db::users::User;
use sultan_db::{Db, devices, org, roles, seed, users};
use sultan_server::{AppState, Clock, NodeRole, SystemClock, credentials, router, token};

const TOP_UP: &str = SEED_REASONS[0];
const SHOP: &str = SEED_REASONS[2];

struct Server {
    root: String,
    device: String,
    branch: String,
    task: tokio::task::JoinHandle<()>,
    data_dir: std::path::PathBuf,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
        let _ = std::fs::remove_dir_all(&self.data_dir);
    }
}

fn user(username: &str, password: Option<&str>, pin: Option<&str>) -> User {
    User {
        id: UserId::generate(),
        username: username.to_owned(),
        display_name: username.to_owned(),
        pin_hash: pin.map(|p| credentials::hash_pin(p).unwrap()),
        password_hash: password.map(|p| credentials::hash_password(p).unwrap()),
        active: true,
    }
}

/// JKT01 with register R1: `owner` (password), Ani (cashier, PIN 1234), Budi (manager, PIN
/// 9999). Answers the server, Ani's and Budi's IDs.
async fn serve() -> (Server, String, String) {
    let db = Db::connect("sqlite::memory:").await.unwrap();
    db.migrate().await.unwrap();
    let now = SystemClock.now();
    seed::seed_defaults(&db, now).await.unwrap();
    let company = Company {
        id: CompanyId::generate(),
        name: "PT Sultan".to_owned(),
    };
    let jkt = Branch {
        id: BranchId::generate(),
        company: company.id,
        code: "JKT01".to_owned(),
        name: "JKT01".to_owned(),
        address: String::new(),
        npwp: None,
        receipt_header: String::new(),
        receipt_footer: String::new(),
        timezone: "Asia/Jakarta".to_owned(),
        archived: false,
    };
    let register = Register {
        id: RegisterId::generate(),
        branch: jkt.id,
        code: "R1".to_owned(),
        name: "Kasir 1".to_owned(),
        default_location: None,
        archived: false,
    };
    let device = token::generate();
    let ani = user("ani", None, Some("1234"));
    let budi = user("budi", None, Some("9999"));
    let (ani_id, budi_id) = (ani.id.to_string(), budi.id.to_string());
    let mut tx = db.begin().await.unwrap();
    org::save_company(&mut tx, &company, now).await.unwrap();
    org::save_branch(&mut tx, &jkt, now).await.unwrap();
    org::save_register(&mut tx, &register, now).await.unwrap();
    for (u, role, scope) in [
        (
            user("owner", Some("owner-pass-1"), None),
            seed::OWNER_ROLE,
            BranchScope::All,
        ),
        (ani, seed::CASHIER_ROLE, BranchScope::Branch(jkt.id)),
        (budi, seed::MANAGER_ROLE, BranchScope::Branch(jkt.id)),
    ] {
        users::save_user(&mut tx, &u, now).await.unwrap();
        roles::replace_assignments(&mut tx, u.id, &[(role, scope)], now)
            .await
            .unwrap();
    }
    devices::insert_device_token(&mut tx, &token::hash(&device), register.id, now)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let data_dir = std::env::temp_dir().join(format!("sultan-client-{}", new_id()));
    let state =
        AppState::new(db, NodeRole::Standalone, Arc::new(SystemClock)).with_data_dir(&data_dir);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        axum::serve(listener, router(state)).await.unwrap();
    });
    (
        Server {
            root: format!("http://{addr}"),
            device,
            branch: jkt.id.to_string(),
            task,
            data_dir,
        },
        ani_id,
        budi_id,
    )
}

fn new_id() -> String {
    ProductId::generate().to_string()
}

#[track_caller]
fn code_of<T: std::fmt::Debug>(result: Result<T, ClientError>) -> (u16, String) {
    let err = result.unwrap_err();
    let p = err.problem().unwrap_or_else(|| panic!("{err:?}"));
    (p.status, p.code.clone())
}

fn cash(kind: &str, amount: i64, reason: Option<&str>) -> CashMovementRequest {
    CashMovementRequest {
        kind: Some(kind.to_owned()),
        amount: Some(amount),
        reason_id: reason.map(str::to_owned),
        ..CashMovementRequest::default()
    }
}

/// Who approves overrides at the till.
struct Approver<'a> {
    id: &'a str,
    pin: &'a str,
}

async fn approve(
    register: &dyn Service,
    token: &str,
    approver: &Approver<'_>,
    action: &str,
    value: Option<i64>,
    context: &str,
) -> String {
    register
        .request_override(
            token,
            &OverrideRequest {
                action: action.to_owned(),
                value,
                context_ref: context.to_owned(),
                approver_id: approver.id.to_owned(),
                approver_pin: approver.pin.to_owned(),
            },
        )
        .await
        .unwrap()
        .token
}

/// The register flow shared by the server and the fake (see the module docs). Answers the Z's ID
/// and the second session's ID (left open).
#[allow(clippy::too_many_lines)] // one flow, step by step
async fn flow(
    register: &dyn Service,
    ani: &str,
    approver: &Approver<'_>,
    branch: &str,
    today: &str,
) -> (String, String) {
    let reasons = register.cash_reasons(ani).await.unwrap();
    assert_eq!(reasons.len(), 6);
    assert_eq!(reasons[0].direction, "in");
    let float = register.proposed_float(ani).await.unwrap();
    assert_eq!((float.proposed_float, float.has_proposal), (0, false));
    assert_eq!(
        code_of(
            register
                .post_cash_movement(ani, &new_id(), &cash("in", 1, Some(TOP_UP)), None)
                .await
        ),
        (409, "no_open_session".to_owned())
    );
    let session = register
        .open_session(ani, 200_000, &new_id())
        .await
        .unwrap();
    // A cash in, its replay and a conflicting body.
    let in_id = new_id();
    let body = cash("in", 20_000, Some(TOP_UP));
    let cash_in = register
        .post_cash_movement(ani, &in_id, &body, None)
        .await
        .unwrap();
    assert_eq!((cash_in.effect, cash_in.kind.as_str()), (20_000, "in"));
    assert!(
        cash_in.number.starts_with("JKT01-R1-K-"),
        "{}",
        cash_in.number
    );
    assert_eq!(
        register
            .post_cash_movement(ani, &in_id, &body, None)
            .await
            .unwrap(),
        cash_in
    );
    assert_eq!(
        code_of(
            register
                .post_cash_movement(ani, &in_id, &cash("in", 1, Some(TOP_UP)), None)
                .await
        ),
        (409, "conflict".to_owned())
    );
    // A cash out above the cashier's limit and a drop need a manager.
    let out_id = new_id();
    let out = cash("out", 75_000, Some(SHOP));
    let err = register
        .post_cash_movement(ani, &out_id, &out, None)
        .await
        .unwrap_err();
    let p = err.problem().unwrap();
    assert_eq!(
        (p.code.as_str(), p.permission(), p.limit()),
        ("needs_override", Some("cash.out"), Some(50_000))
    );
    let t = approve(register, ani, approver, "cash.out", Some(75_000), &out_id).await;
    let cash_out = register
        .post_cash_movement(ani, &out_id, &out, Some(&t))
        .await
        .unwrap();
    assert_eq!(cash_out.approved_by.as_deref(), Some(approver.id));
    let drop_id = new_id();
    let drop = cash("drop", 30_000, None);
    assert_eq!(
        code_of(
            register
                .post_cash_movement(ani, &drop_id, &drop, None)
                .await
        ),
        (403, "needs_override".to_owned())
    );
    let t = approve(register, ani, approver, "cash.drop", None, &drop_id).await;
    register
        .post_cash_movement(ani, &drop_id, &drop, Some(&t))
        .await
        .unwrap();
    // A reversal of the cash in, once.
    let reversal = CashMovementRequest {
        reverses: Some(in_id.clone()),
        note: Some("salah input".to_owned()),
        ..CashMovementRequest::default()
    };
    let back = register
        .post_cash_movement(ani, &new_id(), &reversal, None)
        .await
        .unwrap();
    assert_eq!(
        (back.effect, back.reverses_number.as_deref()),
        (-20_000, Some(cash_in.number.as_str()))
    );
    assert_eq!(
        code_of(
            register
                .post_cash_movement(ani, &new_id(), &reversal, None)
                .await
        ),
        (409, "already_reversed".to_owned())
    );
    let err = register
        .post_cash_movement(ani, &new_id(), &cash("in", 0, Some(TOP_UP)), None)
        .await
        .unwrap_err();
    assert_eq!(err.problem().unwrap().field(), Some("amount"));
    // Reads and slips.
    let read = register.cash_movement(ani, &in_id).await.unwrap();
    assert_eq!(read.reversed_by.as_deref(), Some(back.id.as_str()));
    let page = register
        .cash_movements(
            ani,
            &CashQuery {
                cashier_session_id: Some(session.id.clone()),
                ..CashQuery::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(page.items.len(), 4);
    assert_eq!(
        code_of(register.cash_movements(ani, &CashQuery::default()).await),
        (403, "permission_denied".to_owned())
    );
    let slip = register
        .cash_slip(ani, &out_id, false, "en", None)
        .await
        .unwrap();
    assert!(slip.text.contains("CASH OUT"), "{}", slip.text);
    let copy = register
        .cash_slip(ani, &out_id, true, "id", None)
        .await
        .unwrap();
    assert!(copy.copy && copy.text.contains("SALINAN"), "{}", copy.text);
    // The X: a cashier needs a manager.
    assert_eq!(
        code_of(register.x_report(ani, &session.id, None).await),
        (403, "needs_override".to_owned())
    );
    let t = approve(
        register,
        ani,
        approver,
        "session.view_expected",
        None,
        &session.id,
    )
    .await;
    let x = register.x_report(ani, &session.id, Some(&t)).await.unwrap();
    assert_eq!((x.expected_cash, x.cash.x_reports), (95_000, 1));
    let t = approve(
        register,
        ani,
        approver,
        "session.view_expected",
        None,
        &session.id,
    )
    .await;
    let text = register
        .x_report_text(ani, &session.id, "en", Some(&t))
        .await
        .unwrap();
    assert!(text.text.contains("X REPORT"), "{}", text.text);
    // The close keeps a float, at most the count.
    let mut close = CloseRequest {
        counted_cash: 95_000,
        counts: None,
        float_kept: Some(200_000),
    };
    let err = register
        .close_session(ani, &session.id, &close, &new_id())
        .await
        .unwrap_err();
    assert_eq!(err.problem().unwrap().field(), Some("float_kept"));
    close.float_kept = Some(50_000);
    let report = register
        .close_session(ani, &session.id, &close, &new_id())
        .await
        .unwrap();
    assert_eq!(report.expected_cash, 95_000);
    assert_eq!(
        (report.float.float_kept, report.float.to_safe),
        (Some(50_000), Some(45_000))
    );
    assert_eq!(
        (
            report.cash.drops.count,
            report.cash.reversals.count,
            report.cash.x_reports
        ),
        (1, 1, 2)
    );
    assert_eq!(report.sales.drops, 30_000);
    let text = register
        .close_report_text(ani, &session.id, true, "id")
        .await
        .unwrap();
    assert!(text.text.contains("LAPORAN TUTUP KAS"), "{}", text.text);
    assert_eq!(
        code_of(register.x_report(ani, &session.id, None).await),
        (409, "session_closed".to_owned())
    );
    let float = register.proposed_float(ani).await.unwrap();
    assert_eq!((float.proposed_float, float.has_proposal), (50_000, true));
    // The Z at the till, with a manager's approval.
    let z_body = ZRunRequest {
        day: today.to_owned(),
        branch_id: None,
    };
    assert_eq!(
        code_of(register.run_z(ani, &z_body, "z-0", None).await),
        (403, "needs_override".to_owned())
    );
    let t = approve(
        register,
        ani,
        approver,
        "day.close",
        None,
        &format!("{branch}:{today}"),
    )
    .await;
    let z = register.run_z(ani, &z_body, "z-1", Some(&t)).await.unwrap();
    assert!(z.report.number.ends_with("-01"), "{}", z.report.number);
    assert_eq!(z.session_ids, std::slice::from_ref(&session.id));
    assert_eq!(z.report.branch.cash_to_deposit, 75_000);
    assert_eq!(register.run_z(ani, &z_body, "z-1", None).await.unwrap(), z);
    let t = approve(
        register,
        ani,
        approver,
        "day.close",
        None,
        &format!("{branch}:{today}"),
    )
    .await;
    assert_eq!(
        code_of(register.run_z(ani, &z_body, "z-2", Some(&t)).await),
        (409, "nothing_to_close".to_owned())
    );
    let text = register
        .z_report_text(ani, &z.id, false, "en")
        .await
        .unwrap();
    assert!(text.text.contains("Z REPORT"), "{}", text.text);
    assert_eq!(
        code_of(register.z_report_text(ani, &z.id, true, "en").await),
        (403, "permission_denied".to_owned())
    );
    assert_eq!(
        code_of(register.pending_days(ani, None).await),
        (403, "permission_denied".to_owned())
    );
    // The next session proposes the float kept.
    let next = register.open_session(ani, 60_000, &new_id()).await.unwrap();
    assert_eq!(next.opening_proposed, Some(50_000));
    (z.id, next.id)
}

/// The back office's part, shared by the server and the fake.
async fn back_office(office: &dyn BackOffice, owner: &str, branch: &str, z_id: &str, open: &str) {
    assert_eq!(office.all_cash_reasons(owner).await.unwrap().len(), 6);
    let id = new_id();
    let body = CashReasonBody {
        name: "Galon".to_owned(),
        direction: "out".to_owned(),
        note_required: false,
        active: true,
        sort: 40,
    };
    let reason = office.put_cash_reason(owner, &id, &body).await.unwrap();
    assert_eq!((reason.name.as_str(), reason.archived), ("Galon", false));
    let archived = office.delete_cash_reason(owner, &id).await.unwrap();
    assert!(archived.archived);
    assert_eq!(
        code_of(office.put_cash_reason(owner, &id, &body).await),
        (409, "archived".to_owned())
    );
    let page = office
        .z_reports(
            owner,
            &ZQuery {
                branch_id: Some(branch.to_owned()),
                ..ZQuery::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(page.items.len(), 1);
    assert_eq!(office.z_report(owner, z_id).await.unwrap().id, z_id);
    let text = office.z_report_text(owner, z_id, true, "en").await.unwrap();
    assert!(text.copy);
    assert_eq!(office.pending_days(owner, Some(branch)).await.unwrap(), []);
    let report = office
        .force_close_session(
            owner,
            open,
            &CloseRequest {
                counted_cash: 60_000,
                counts: None,
                float_kept: None,
            },
            &new_id(),
        )
        .await
        .unwrap();
    assert!(report.forced);
    assert_eq!(report.float.float_kept, Some(60_000));
    assert_eq!(report.float.opening_proposed, Some(50_000));
}

fn png(width: u32) -> Vec<u8> {
    let img = image::DynamicImage::ImageRgb8(image::RgbImage::new(width, 4));
    let mut out = Vec::new();
    img.write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)
        .unwrap();
    out
}

/// Nota photos and the missing-nota setting (phase K6, spec 2.7 §2a), shared by the server and
/// the fake. Runs after [`back_office`] (no session open).
async fn photos(register: &dyn Service, office: &dyn BackOffice, ani: &str, owner: &str) -> String {
    let session = register.open_session(ani, 10_000, &new_id()).await.unwrap();
    let out = register
        .post_cash_movement(ani, &new_id(), &cash("out", 20_000, Some(SHOP)), None)
        .await
        .unwrap();
    assert!(!out.missing_nota);
    // The threshold: kept when left out, lowered to flag the Rp 20 000 cash out.
    let mut settings = office.sales_settings(owner).await.unwrap();
    assert_eq!(settings.nota_flag_above, Some(100_000));
    settings.nota_flag_above = None;
    let kept = office.put_sales_settings(owner, &settings).await.unwrap();
    assert_eq!(kept.nota_flag_above, Some(100_000));
    settings.nota_flag_above = Some(-1);
    assert_eq!(
        code_of(office.put_sales_settings(owner, &settings).await),
        (422, "invalid_field".to_owned())
    );
    settings.nota_flag_above = Some(10_000);
    let lowered = office.put_sales_settings(owner, &settings).await.unwrap();
    assert_eq!(lowered.nota_flag_above, Some(10_000));
    assert!(
        register
            .cash_movement(ani, &out.id)
            .await
            .unwrap()
            .missing_nota
    );

    // A photo clears the flag; its replay answers the same photo.
    let photo_id = new_id();
    let photo = register
        .add_cash_photo(ani, &out.id, &photo_id, png(8))
        .await
        .unwrap();
    assert_eq!(
        (
            photo.id.as_str(),
            photo.cash_movement_id.as_str(),
            photo.sha256.len()
        ),
        (photo_id.as_str(), out.id.as_str(), 64)
    );
    assert_eq!(
        register
            .add_cash_photo(ani, &out.id, &photo_id, png(8))
            .await
            .unwrap(),
        photo
    );
    let read = register.cash_movement(ani, &out.id).await.unwrap();
    assert_eq!(read.photo_ids, vec![photo_id.clone()]);
    assert!(!read.missing_nota);
    let bytes = register.cash_photo(ani, &out.id, &photo_id).await.unwrap();
    assert!(bytes.len() > 8, "{} bytes", bytes.len());
    assert_eq!(
        code_of(register.cash_photo(ani, &out.id, &new_id()).await),
        (404, "not_found".to_owned())
    );
    // Not an image; the fourth photo. (Over 10 MB: the server's API tests, and the fake's below.)
    assert_eq!(
        code_of(
            register
                .add_cash_photo(ani, &out.id, &new_id(), b"hello".to_vec())
                .await
        ),
        (422, "invalid_image".to_owned())
    );
    for width in [9, 10] {
        register
            .add_cash_photo(ani, &out.id, &new_id(), png(width))
            .await
            .unwrap();
    }
    let err = register
        .add_cash_photo(ani, &out.id, &new_id(), png(11))
        .await
        .unwrap_err();
    let p = err.problem().unwrap();
    assert_eq!((p.status, p.code.as_str()), (409, "photo_limit"));
    assert_eq!(
        code_of(
            register
                .add_cash_photo(ani, &new_id(), &new_id(), png(8))
                .await
        ),
        (404, "not_found".to_owned())
    );
    // A second cash out above the threshold, without a photo: the close counts it.
    let bare = register
        .post_cash_movement(ani, &new_id(), &cash("out", 15_000, Some(SHOP)), None)
        .await
        .unwrap();
    assert!(bare.missing_nota);
    session.id
}

/// Closes `session` (counted 0); answers the close report's missing-nota count.
async fn close_photos(register: &dyn Service, ani: &str, session: &str) -> u32 {
    let body = CloseRequest {
        counted_cash: 0,
        counts: None,
        float_kept: None,
    };
    let report = register
        .close_session(ani, session, &body, &new_id())
        .await
        .unwrap();
    report.cash.missing_nota
}

fn today(now_ms: i64) -> String {
    let day = LocalDay::of(
        sultan_core::time::Timestamp::from_unix_millis(now_ms),
        BranchTimeZone::default(),
    );
    let (y, m, d) = day.ymd();
    format!("{y:04}-{m:02}-{d:02}")
}

#[tokio::test]
async fn cash_calls_over_http() {
    let (server, ani_id, budi_id) = serve().await;
    let register = HttpService::new(&server.root, &server.device);
    let ani = register.pin_login(&ani_id, "1234").await.unwrap().token;
    let approver = Approver {
        id: &budi_id,
        pin: "9999",
    };
    let day = today(SystemClock.now().unix_millis());
    let (z, open) = flow(&register, &ani, &approver, &server.branch, &day).await;
    let office = HttpService::user(&server.root);
    let owner = office
        .password_login("owner", "owner-pass-1")
        .await
        .unwrap()
        .token;
    back_office(&office, &owner, &server.branch, &z, &open).await;
    let session = photos(&register, &office, &ani, &owner).await;
    assert_eq!(close_photos(&register, &ani, &session).await, 1);
    // After the close the cashier adds no photo at the till.
    let query = CashQuery {
        cashier_session_id: Some(session.clone()),
        ..CashQuery::default()
    };
    let page = register.cash_movements(&ani, &query).await.unwrap();
    assert_eq!(
        code_of(
            register
                .add_cash_photo(&ani, &page.items[0].id, &new_id(), png(8))
                .await
        ),
        (403, "permission_denied".to_owned())
    );
    // The payout slip of an unknown entry.
    assert_eq!(
        code_of(
            register
                .payout_slip(ani.as_str(), &new_id(), &new_id(), false, "en", None)
                .await
        ),
        (404, "not_found".to_owned())
    );
}

#[tokio::test]
#[allow(clippy::too_many_lines)] // one flow, step by step
async fn cash_calls_on_the_fake() {
    let fake = FakeService::default();
    let ani = fake.pin_login("u-ani", "1234").await.unwrap().token;
    let approver = Approver {
        id: "u-budi",
        pin: "9999",
    };
    let (z, open) = flow(
        &fake,
        &ani,
        &approver,
        sultan_client::fake::BRANCH_ID,
        &today(1_800_000_000_000),
    )
    .await;
    assert_eq!(fake.cash_movements_posted().len(), 4);
    let owner = fake
        .password_login("owner", "owner-pass-1")
        .await
        .unwrap()
        .token;
    back_office(&fake, &owner, sultan_client::fake::BRANCH_ID, &z, &open).await;
    let session = photos(&fake, &fake, &ani, &owner).await;
    let posted = fake.cash_movements_posted();
    let out = posted[posted.len() - 2].clone();
    assert_eq!(
        code_of(
            fake.add_cash_photo(&ani, &out.id, &new_id(), vec![0; 11 * 1024 * 1024])
                .await
        ),
        (409, "photo_limit".to_owned())
    );
    // Another movement: over 10 MB, and a WebP / JPEG are images too.
    let other = fake
        .post_cash_movement(&ani, &new_id(), &cash("out", 1_000, Some(SHOP)), None)
        .await
        .unwrap();
    assert_eq!(
        code_of(
            fake.add_cash_photo(&ani, &other.id, &new_id(), vec![0; 11 * 1024 * 1024])
                .await
        ),
        (413, "payload_too_large".to_owned())
    );
    for bytes in [
        b"RIFF\0\0\0\0WEBPVP8 ".to_vec(),
        vec![0xff, 0xd8, 0xff, 0xe0],
    ] {
        fake.add_cash_photo(&ani, &other.id, &new_id(), bytes)
            .await
            .unwrap();
    }
    // A used photo ID on another movement: conflict.
    assert_eq!(
        code_of(
            fake.add_cash_photo(&ani, &other.id, &out.photo_ids[0], png(8))
                .await
        ),
        (409, "conflict".to_owned())
    );
    // A cashier in the back office manages no reasons and runs no Z.
    let kasir = fake
        .password_login("kasir", "kasir-pass-1")
        .await
        .unwrap()
        .token;
    assert_eq!(
        code_of(fake.all_cash_reasons(&kasir).await),
        (403, "permission_denied".to_owned())
    );
    assert_eq!(
        code_of(
            fake.add_cash_photo(&kasir, &other.id, &new_id(), png(8))
                .await
        ),
        (403, "permission_denied".to_owned())
    );
    // The close counts the one cash out still missing its nota; after it the cashier adds none.
    assert_eq!(close_photos(&fake, &ani, &session).await, 1);
    assert_eq!(
        code_of(
            fake.add_cash_photo(&ani, &other.id, &new_id(), png(8))
                .await
        ),
        (403, "permission_denied".to_owned())
    );
    let jpeg = fake.cash_photo(&owner, &out.id, &out.photo_ids[0]).await;
    assert_eq!(jpeg.unwrap(), png(8));
    let body = ZRunRequest {
        day: today(1_800_000_000_000),
        branch_id: Some(sultan_client::fake::BRANCH_ID.to_owned()),
    };
    assert_eq!(
        code_of(fake.run_z(&kasir, &body, "k", None).await),
        (403, "permission_denied".to_owned())
    );
    assert_eq!(
        code_of(
            fake.run_z(
                &owner,
                &ZRunRequest {
                    day: "2027-1-1".into(),
                    branch_id: None
                },
                "k2",
                None
            )
            .await
        ),
        (422, "invalid_field".to_owned())
    );
    // A payout and its slip (Budi, a manager, at the till).
    let budi = fake.pin_login("u-budi", "9999").await.unwrap().token;
    let session = fake.open_session(&budi, 0, "b-open").await.unwrap();
    let payout = fake
        .balance_payout(&budi, sultan_client::fake::SEED_BUDI, 1_000, "pay-1")
        .await
        .unwrap();
    let slip = fake
        .payout_slip(
            &budi,
            sultan_client::fake::SEED_BUDI,
            &payout.entry.id,
            true,
            "en",
            None,
        )
        .await
        .unwrap();
    assert!(slip.text.contains("BALANCE PAYOUT"), "{}", slip.text);
    // The manager reads every movement and the X of his own session without approval.
    let x = fake.x_report(&budi, &session.id, None).await.unwrap();
    assert_eq!(x.sales.payouts, 1_000);
    let all = fake
        .cash_movements(&budi, &CashQuery::default())
        .await
        .unwrap();
    assert_eq!(all.items.len(), 7);
    assert_eq!(
        code_of(
            fake.payout_slip(&budi, "nobody", "nothing", false, "en", None)
                .await
        ),
        (404, "not_found".to_owned())
    );
}

#[tokio::test]
async fn a_fake_z_takes_the_sessions_sales_documents() {
    let fake = FakeService::default();
    let demo = fake.seed_sales_demo(4).await;
    let ani = fake.pin_login("u-ani", "1234").await.unwrap().token;
    let session = fake.current_session(&ani).await.unwrap().unwrap();
    let close = CloseRequest {
        counted_cash: 0,
        counts: None,
        float_kept: None,
    };
    fake.close_session(&ani, &session.id, &close, "close")
        .await
        .unwrap();
    let owner = fake
        .password_login("owner", "owner-pass-1")
        .await
        .unwrap()
        .token;
    // The back office lists the session with its cashier and register (spec 2.7 K5).
    let page = fake
        .sessions(&owner, &sultan_client::SessionQuery::default())
        .await
        .unwrap();
    assert_eq!(page.items[0].cashier_name.as_deref(), Some("Ani"));
    assert_eq!(page.items[0].register_code.as_deref(), Some("R1"));
    let detail = Service::session(&fake, &owner, &session.id).await.unwrap();
    assert!(detail.close_report.is_some());
    let body = ZRunRequest {
        day: today(1_800_000_000_000),
        branch_id: Some(sultan_client::fake::BRANCH_ID.to_owned()),
    };
    let z = fake.run_z(&owner, &body, "z", None).await.unwrap();
    let t = &z.report.branch;
    assert_eq!(t.sales_count, 4, "the voided sale counts in its session");
    let gross: i64 = demo
        .sales
        .iter()
        .flat_map(|s| &s.lines)
        .map(|l| l.gross)
        .sum();
    assert_eq!(t.gross_sales, gross);
    assert_eq!(
        t.first_sale.as_ref().map(|s| s.number.as_str()),
        Some(demo.sales[0].summary.number.as_str())
    );
    assert_eq!(
        t.last_sale.as_ref().map(|s| s.number.as_str()),
        Some(demo.sales[3].summary.number.as_str())
    );
    // The tax summary: the sales minus the post-void and the return.
    let sold: i64 = demo
        .sales
        .iter()
        .flat_map(|s| &s.lines)
        .map(|l| l.tax)
        .sum();
    let voided: i64 = demo.sales[1].lines.iter().map(|l| l.tax).sum();
    let tax: i64 = t.tax.iter().map(|l| l.tax).sum();
    assert!(sold > 0, "the demo product is taxed");
    assert!(tax < sold - voided, "the return takes its tax back: {tax}");
    let text = fake
        .z_report_text(&owner, &z.id, false, "en")
        .await
        .unwrap();
    assert!(
        text.text.contains(&demo.sales[0].summary.number),
        "{}",
        text.text
    );
}
