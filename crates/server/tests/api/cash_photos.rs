//! Nota photos over HTTP (spec 2.7 §2a, §11; phase K6): spec 2.7 §12 server cases 31 (the
//! upload pipeline and its limits), 32 (who may add a photo, audit, outbox, file mode), 33 (the
//! missing-nota flag with `nota_flag_above`, in lists, the close report and the Z) and 34 (the
//! file is written before the row; an orphan file is reused).

#![allow(clippy::too_many_lines)]

use std::io::Cursor;
use std::path::{Path, PathBuf};

use axum::http::{Method, StatusCode};
use image::{DynamicImage, ImageFormat, RgbImage};
use serde_json::{Value, json};
use sultan_core::access::BranchScope;
use sultan_db::seed;
use sultan_server::attachments::{self, Attachments};
use sultan_server::product_image::{self, NOTA};
use sultan_server::router;

use crate::cash::{PARKING, SHOP, cash, close, get_as, move_cash};
use crate::common::{ANI_PIN, BUDI_PIN, Harness, RawResponse, api_tests, new_id};
use crate::sales::open;

api_tests!(
    photos_are_scaled_stored_once_and_limited,
    who_may_add_a_photo,
    missing_nota_follows_the_setting,
    files_are_written_before_the_row,
);

fn image(width: u32, height: u32, format: ImageFormat) -> Vec<u8> {
    let img = DynamicImage::ImageRgb8(RgbImage::from_fn(width, height, |x, y| {
        image::Rgb([x.to_le_bytes()[0], y.to_le_bytes()[0], 128])
    }));
    let mut out = Vec::new();
    img.write_to(&mut Cursor::new(&mut out), format).unwrap();
    out
}

/// `PUT /cash-movements/{movement}/photos/{photo}` with `headers` (bearer, device token).
async fn put_photo(
    h: &Harness,
    headers: &[(&str, &str)],
    movement: &str,
    photo: &str,
    bytes: Vec<u8>,
) -> RawResponse {
    h.send_raw(
        Method::PUT,
        &format!("/api/v1/cash-movements/{movement}/photos/{photo}"),
        headers,
        bytes,
    )
    .await
}

#[track_caller]
fn assert_problem(r: &RawResponse, status: u16, code: &str) {
    let body = r.json();
    assert_eq!(
        (r.status.as_u16(), body["code"].as_str()),
        (status, Some(code)),
        "{body}"
    );
}

/// Every file under `dir`, recursively.
fn files(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for e in entries {
        let path = e.unwrap().path();
        if path.is_dir() {
            out.extend(files(&path));
        } else {
            out.push(path);
        }
    }
    out
}

fn attachments_of(h: &Harness) -> PathBuf {
    h.data_dir.0.join("attachments")
}

async fn photos_are_scaled_stored_once_and_limited(h: &Harness) {
    let budi = h.pin_token(&h.jkt_device, &h.budi, BUDI_PIN).await;
    let at = (budi.as_str(), h.jkt_device.as_str());
    let bearer = format!("Bearer {budi}");
    let till = [
        ("authorization", bearer.as_str()),
        ("x-register-token", h.jkt_device.as_str()),
    ];
    open(h, at, 200_000).await;
    let m = move_cash(h, at, cash("out", 150_000, Some(SHOP))).await;
    let mid = m["id"].as_str().unwrap();
    assert_eq!(m["missing_nota"], true);

    // Case 31: a photo larger than 1600 px is scaled down and stored as a JPEG ≤ 400 KB.
    let pid = new_id();
    let r = put_photo(h, &till, mid, &pid, image(2_000, 1_500, ImageFormat::Png)).await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.json());
    let body = r.json();
    assert_eq!(body["id"], pid.as_str());
    assert_eq!(body["cash_movement_id"], mid);
    assert_eq!(body["added_by"], h.budi.id.to_string());
    let size = body["size"].as_u64().unwrap();
    assert!(size > 0 && size <= NOTA.max_bytes as u64, "{size}");
    let stored = files(&attachments_of(h));
    assert_eq!(stored.len(), 1, "{stored:?}");
    let sha = body["sha256"].as_str().unwrap();
    assert_eq!(
        stored[0],
        attachments_of(h).join(&sha[..2]).join(format!("{sha}.jpg"))
    );
    let bytes = std::fs::read(&stored[0]).unwrap();
    assert_eq!(bytes.len() as u64, size);
    let decoded = image::load_from_memory(&bytes).unwrap();
    assert_eq!(image::guess_format(&bytes).unwrap(), ImageFormat::Jpeg);
    assert_eq!((decoded.width(), decoded.height()), (1_600, 1_200));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&stored[0]).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    // The movement lists the photo and is no longer missing its nota.
    let r = get_as(h, at, &format!("/api/v1/cash-movements/{mid}")).await;
    assert_eq!(r.body["photo_ids"], json!([pid]));
    assert_eq!(r.body["missing_nota"], false);

    // The same photo ID and content again: 200, nothing new.
    let outbox = h.outbox_count().await;
    let r = put_photo(h, &till, mid, &pid, image(2_000, 1_500, ImageFormat::Png)).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json());
    assert_eq!(r.json()["sha256"], sha);
    assert_eq!(h.outbox_count().await, outbox);
    assert_eq!(files(&attachments_of(h)).len(), 1);
    // The same ID with another image: conflict.
    let r = put_photo(h, &till, mid, &pid, image(30, 20, ImageFormat::Png)).await;
    assert_problem(&r, 409, "conflict");

    // A text file, an 11 MB body, a photo ID that is not a UUIDv7, an unknown movement.
    let r = put_photo(h, &till, mid, &new_id(), b"just a note".to_vec()).await;
    assert_problem(&r, 422, "invalid_image");
    let r = put_photo(h, &till, mid, &new_id(), vec![0u8; 11 * 1024 * 1024]).await;
    assert_problem(&r, 413, "payload_too_large");
    let nil = uuid::Uuid::nil().to_string();
    let r = put_photo(h, &till, mid, &nil, image(8, 8, ImageFormat::Png)).await;
    assert_problem(&r, 400, "invalid_body");
    let r = put_photo(
        h,
        &till,
        &new_id(),
        &new_id(),
        image(8, 8, ImageFormat::Png),
    )
    .await;
    assert_problem(&r, 404, "not_found");

    // A JPEG and a WebP make three photos; a fourth is refused.
    for format in [ImageFormat::Jpeg, ImageFormat::WebP] {
        let r = put_photo(h, &till, mid, &new_id(), image(40, 30, format)).await;
        assert_eq!(r.status, StatusCode::CREATED, "{format:?} {}", r.json());
    }
    let r = put_photo(h, &till, mid, &new_id(), image(41, 30, ImageFormat::Png)).await;
    assert_problem(&r, 409, "photo_limit");
    assert_eq!(r.json()["max"], 3);
    // A replay of a stored photo still answers 200 at the limit.
    let r = put_photo(h, &till, mid, &pid, image(2_000, 1_500, ImageFormat::Png)).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json());
    let r = get_as(h, at, &format!("/api/v1/cash-movements/{mid}")).await;
    assert_eq!(r.body["photo_ids"].as_array().unwrap().len(), 3);

    // The read: the JPEG with an ETag, 304 when it matches.
    let uri = format!("/api/v1/cash-movements/{mid}/photos/{pid}");
    let r = h.send_raw(Method::GET, &uri, &till, Vec::new()).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.headers["content-type"], "image/jpeg");
    assert_eq!(r.bytes, bytes);
    let etag = r.headers["etag"].to_str().unwrap().to_owned();
    assert_eq!(etag, product_image::etag(&bytes));
    let mut with_tag = till.to_vec();
    with_tag.push(("if-none-match", etag.as_str()));
    let r = h.send_raw(Method::GET, &uri, &with_tag, Vec::new()).await;
    assert_eq!(r.status, StatusCode::NOT_MODIFIED);
    // Another user without `report.sales`: refused; an unknown photo: 404.
    let cici = h.pin_token(&h.bdg_device, &h.cici, ANI_PIN).await;
    let cici_bearer = format!("Bearer {cici}");
    let other = [
        ("authorization", cici_bearer.as_str()),
        ("x-register-token", h.bdg_device.as_str()),
    ];
    let r = h.send_raw(Method::GET, &uri, &other, Vec::new()).await;
    assert_problem(&r, 403, "permission_denied");
    let r = h
        .send_raw(
            Method::GET,
            &format!("/api/v1/cash-movements/{mid}/photos/{}", new_id()),
            &till,
            Vec::new(),
        )
        .await;
    assert_problem(&r, 404, "not_found");
    // A photo row whose file is not on this node (an HQ copy before the bytes sync): 404.
    std::fs::remove_file(&stored[0]).unwrap();
    let r = h.send_raw(Method::GET, &uri, &till, Vec::new()).await;
    assert_problem(&r, 404, "not_found");
}

async fn who_may_add_a_photo(h: &Harness) {
    let ani = h.pin_token(&h.jkt_device, &h.ani, ANI_PIN).await;
    let at = (ani.as_str(), h.jkt_device.as_str());
    let ani_bearer = format!("Bearer {ani}");
    let till = [
        ("authorization", ani_bearer.as_str()),
        ("x-register-token", h.jkt_device.as_str()),
    ];
    let session = open(h, at, 200_000).await;
    let m = move_cash(h, at, cash("out", 20_000, Some(PARKING))).await;
    let mid = m["id"].as_str().unwrap().to_owned();
    let photo = || image(16, 12, ImageFormat::Png);
    let mut events = h.events.subscribe();
    let (outbox, audits) = (
        h.outbox_count().await,
        h.audit_count("cash_movement.photo").await,
    );
    // Case 32: the movement's own cashier while the session is open: 201, one audit and one
    // outbox row, a `cash.moved` event.
    let r = put_photo(h, &till, &mid, &new_id(), photo()).await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.json());
    assert_eq!(h.outbox_count().await, outbox + 1);
    assert_eq!(h.audit_count("cash_movement.photo").await, audits + 1);
    let event = events.try_recv().unwrap();
    assert_eq!(event.kind, "cash.moved");
    assert_eq!(event.data["movement_id"], mid.as_str());

    // Another user at the till (a manager, who holds `cash.out`): 403.
    let budi = h.pin_token(&h.jkt_device, &h.budi, BUDI_PIN).await;
    let budi_bearer = format!("Bearer {budi}");
    let budi_till = [
        ("authorization", budi_bearer.as_str()),
        ("x-register-token", h.jkt_device.as_str()),
    ];
    let r = put_photo(h, &budi_till, &mid, &new_id(), photo()).await;
    assert_problem(&r, 403, "permission_denied");
    // A cashier of another branch: 403.
    let cici = h.pin_token(&h.bdg_device, &h.cici, ANI_PIN).await;
    let cici_bearer = format!("Bearer {cici}");
    let r = put_photo(
        h,
        &[
            ("authorization", cici_bearer.as_str()),
            ("x-register-token", h.bdg_device.as_str()),
        ],
        &mid,
        &new_id(),
        photo(),
    )
    .await;
    assert_problem(&r, 403, "permission_denied");

    // After the session closed, the cashier at the till: 403.
    close(h, at, &session, json!({ "counted_cash": 180_000 })).await;
    let r = put_photo(h, &till, &mid, &new_id(), photo()).await;
    assert_problem(&r, 403, "permission_denied");

    // In the back office: `cash.out` at the branch (a manager) → 201; a viewer → 403.
    let manager = h.manager_token().await;
    let office = format!("Bearer {manager}");
    let r = put_photo(
        h,
        &[("authorization", office.as_str())],
        &mid,
        &new_id(),
        photo(),
    )
    .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.json());
    h.add_user("vera", seed::VIEWER_ROLE, BranchScope::Branch(h.jkt.id))
        .await;
    let vera = h.password_token("vera", crate::common::BUDI_PASSWORD).await;
    let vera = format!("Bearer {vera}");
    let r = put_photo(
        h,
        &[("authorization", vera.as_str())],
        &mid,
        &new_id(),
        photo(),
    )
    .await;
    assert_problem(&r, 403, "permission_denied");
    // The viewer reads the photos (`report.sales`).
    let r = h
        .call(
            &manager,
            Method::GET,
            &format!("/api/v1/cash-movements/{mid}"),
            None,
        )
        .await;
    let ids = r.body["photo_ids"].as_array().unwrap().clone();
    assert_eq!(ids.len(), 2);
    let r = h
        .send_raw(
            Method::GET,
            &format!(
                "/api/v1/cash-movements/{mid}/photos/{}",
                ids[1].as_str().unwrap()
            ),
            &[("authorization", vera.as_str())],
            Vec::new(),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK);
}

async fn put_settings(h: &Harness, owner: &str, flag: Value) -> crate::common::Response {
    h.call(
        owner,
        Method::PUT,
        "/api/v1/settings/sales",
        Some(json!({ "return_days": 30, "nota_flag_above": flag })),
    )
    .await
}

async fn missing_nota_follows_the_setting(h: &Harness) {
    let owner = h.owner_token().await;
    // The setting: default 100 000, kept when left out, never negative.
    let r = h
        .call(&owner, Method::GET, "/api/v1/settings/sales", None)
        .await;
    assert_eq!(r.body["nota_flag_above"], 100_000);
    put_settings(h, &owner, json!(-1))
        .await
        .assert_problem(422, "invalid_field");
    let r = h
        .call(
            &owner,
            Method::PUT,
            "/api/v1/settings/sales",
            Some(json!({ "return_days": 14 })),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert_eq!(
        (
            r.body["return_days"].as_i64(),
            r.body["nota_flag_above"].as_i64()
        ),
        (Some(14), Some(100_000))
    );

    // Case 33.
    let budi = h.pin_token(&h.jkt_device, &h.budi, BUDI_PIN).await;
    let at = (budi.as_str(), h.jkt_device.as_str());
    let bearer = format!("Bearer {budi}");
    let till = [
        ("authorization", bearer.as_str()),
        ("x-register-token", h.jkt_device.as_str()),
    ];
    let session = open(h, at, 500_000).await;
    let mut ids = Vec::new();
    for amount in [99_999, 100_000, 150_000, 120_000] {
        let m = move_cash(h, at, cash("out", amount, Some(SHOP))).await;
        ids.push(m["id"].as_str().unwrap().to_owned());
    }
    let r = put_photo(h, &till, &ids[2], &new_id(), image(8, 8, ImageFormat::Png)).await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.json());
    // The 120 000 one is reversed (a manager holds `cash.out` up to Rp 2 000 000).
    move_cash(h, at, json!({ "reverses": ids[3], "note": "salah" })).await;
    let missing = |r: &crate::common::Response| -> Vec<String> {
        r.body["items"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|m| m["missing_nota"] == true)
            .map(|m| m["id"].as_str().unwrap().to_owned())
            .collect()
    };
    let list = format!("/api/v1/cash-movements?cashier_session_id={session}");
    let r = get_as(h, at, &list).await;
    assert_eq!(missing(&r), vec![ids[1].clone()]);
    let r = h
        .call(
            &owner,
            Method::GET,
            &format!(
                "/api/v1/cash-movements?branch_id={}&missing_nota=true",
                h.jkt.id
            ),
            None,
        )
        .await;
    let listed: Vec<&str> = r.body["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap())
        .collect();
    assert_eq!(listed, vec![ids[1].as_str()]);
    // `0` turns the flag off.
    assert_eq!(
        put_settings(h, &owner, json!(0)).await.status,
        StatusCode::OK
    );
    assert!(missing(&get_as(h, at, &list).await).is_empty());
    // A threshold above every cash out: none either; back to 100 000.
    put_settings(h, &owner, json!(1_000_000)).await;
    assert!(missing(&get_as(h, at, &list).await).is_empty());
    let r = put_settings(h, &owner, json!(100_000)).await;
    assert_eq!(r.body["nota_flag_above"], 100_000);

    // The close report and the Z count it.
    let report = close(h, at, &session, json!({ "counted_cash": 130_001 })).await;
    assert_eq!(report["cash"]["missing_nota"], 1, "{report}");
    let r = h
        .send(
            Method::POST,
            "/api/v1/z-reports",
            &[
                ("authorization", &format!("Bearer {owner}")),
                ("idempotency-key", "z-nota"),
            ],
            Some(json!({ "day": "2027-01-15", "branch_id": h.jkt.id.to_string() })),
        )
        .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
    assert_eq!(r.body["report"]["branch"]["cash"]["missing_nota"], 1);
}

async fn files_are_written_before_the_row(h: &Harness) {
    let ani = h.pin_token(&h.jkt_device, &h.ani, ANI_PIN).await;
    let at = (ani.as_str(), h.jkt_device.as_str());
    let bearer = format!("Bearer {ani}");
    let till = [
        ("authorization", bearer.as_str()),
        ("x-register-token", h.jkt_device.as_str()),
    ];
    open(h, at, 100_000).await;
    let m = move_cash(h, at, cash("out", 10_000, Some(PARKING))).await;
    let mid = m["id"].as_str().unwrap().to_owned();
    let upload = image(24, 18, ImageFormat::Png);

    // Case 34: the file cannot be written (the data directory is a file): 500, no row, no
    // audit, no outbox.
    let blocked = h.data_dir.0.join("blocked");
    std::fs::write(&blocked, b"x").unwrap();
    let app = router(h.state(h.node_role).with_data_dir(&blocked));
    let outbox = h.outbox_count().await;
    let r = Harness::send_raw_to(
        app,
        Method::PUT,
        &format!("/api/v1/cash-movements/{mid}/photos/{}", new_id()),
        &till,
        upload.clone(),
    )
    .await;
    assert_problem(&r, 500, "internal");
    assert_eq!(h.outbox_count().await, outbox);
    assert_eq!(h.audit_count("cash_movement.photo").await, 0);
    let r = get_as(h, at, &format!("/api/v1/cash-movements/{mid}")).await;
    assert_eq!(r.body["photo_ids"], json!([]));

    // A crash after the file write leaves an orphan file; the next upload of the same content
    // reuses it (one file).
    let processed = product_image::process_with(&upload, NOTA).unwrap();
    let store = Attachments::new(&h.data_dir.0);
    let hash = store.store(&processed.data).unwrap();
    let orphan = store.path(&hash);
    assert_eq!(files(&attachments_of(h)), vec![orphan.clone()]);
    let r = put_photo(h, &till, &mid, &new_id(), upload).await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.json());
    assert_eq!(r.json()["sha256"], attachments::hex(&hash));
    assert_eq!(files(&attachments_of(h)), vec![orphan]);

    // A node without a data directory answers 500 too.
    let app = router(sultan_server::AppState::new(
        h.db.clone(),
        h.node_role,
        std::sync::Arc::new(h.clock.clone()),
    ));
    let r = Harness::send_raw_to(
        app,
        Method::PUT,
        &format!("/api/v1/cash-movements/{mid}/photos/{}", new_id()),
        &till,
        image(5, 5, ImageFormat::Png),
    )
    .await;
    assert_problem(&r, 500, "internal");
}
