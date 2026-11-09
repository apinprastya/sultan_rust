//! Product images over HTTP (spec 2.2 §8, §14 server case 21; phase C3).

use std::io::Cursor;

use axum::http::{Method, StatusCode};
use image::{DynamicImage, ImageFormat, RgbImage};

use crate::catalog_fixtures::{create, simple};
use crate::common::{Harness, api_tests, new_id};

api_tests!(upload_resize_download_and_delete, bad_uploads_are_rejected);

fn png(width: u32, height: u32) -> Vec<u8> {
    let image = DynamicImage::ImageRgb8(RgbImage::from_fn(width, height, |x, y| {
        image::Rgb([x.to_le_bytes()[0], y.to_le_bytes()[0], 128])
    }));
    let mut out = Vec::new();
    image
        .write_to(&mut Cursor::new(&mut out), ImageFormat::Png)
        .unwrap();
    out
}

/// §14 item 21: a 2000×1000 PNG is stored as a JPEG of 512×256 and at most 256 KB.
async fn upload_resize_download_and_delete(h: &Harness) {
    let owner = h.owner_token().await;
    let doc = simple("FOTO1", "FOTO1");
    create(h, &owner, &doc).await;
    let uri = format!("/api/v1/products/{}/image", doc.id);
    let bearer = format!("Bearer {owner}");
    let auth = [
        ("authorization", bearer.as_str()),
        ("content-type", "image/png"),
    ];
    let outbox = h.outbox_count().await;
    let r = h
        .send_raw(Method::PUT, &uri, &auth, png(2_000, 1_000))
        .await;
    assert_eq!(r.status, StatusCode::CREATED, "{:?}", r.json());
    let meta = r.json();
    assert_eq!(meta["mime"], "image/jpeg");
    assert_eq!(
        (meta["width"].as_u64(), meta["height"].as_u64()),
        (Some(512), Some(256))
    );
    assert!(meta["bytes"].as_u64().unwrap() <= 256 * 1024);
    assert_eq!(h.audit_count("catalog.product.image.put").await, 1);
    assert_eq!(h.outbox_count().await, outbox + 1);

    // Download with the device token: JPEG bytes with an ETag.
    let r = h
        .send_raw(
            Method::GET,
            &uri,
            &[("x-register-token", &h.jkt_device)],
            vec![],
        )
        .await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.headers["content-type"], "image/jpeg");
    let etag = r.headers["etag"].to_str().unwrap().to_owned();
    assert_eq!(etag, meta["etag"]);
    assert_eq!(r.bytes.len() as u64, meta["bytes"].as_u64().unwrap());
    let decoded = image::load_from_memory(&r.bytes).unwrap();
    assert_eq!((decoded.width(), decoded.height()), (512, 256));
    let r = h
        .send_raw(
            Method::GET,
            &uri,
            &[
                ("x-register-token", &h.jkt_device),
                ("if-none-match", &etag),
            ],
            vec![],
        )
        .await;
    assert_eq!(r.status, StatusCode::NOT_MODIFIED);
    assert!(r.bytes.is_empty());
    let r = h
        .send_raw(
            Method::GET,
            &uri,
            &[("authorization", &bearer), ("if-none-match", "\"other\"")],
            vec![],
        )
        .await;
    assert_eq!(r.status, StatusCode::OK);

    // The same image again: 200, nothing written. A small image is never scaled up.
    let r = h
        .send_raw(Method::PUT, &uri, &auth, png(2_000, 1_000))
        .await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(h.audit_count("catalog.product.image.put").await, 1);
    let r = h.send_raw(Method::PUT, &uri, &auth, png(300, 120)).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(
        (r.json()["width"].as_u64(), r.json()["height"].as_u64()),
        (Some(300), Some(120))
    );

    let r = h.call(&owner, Method::DELETE, &uri, None).await;
    assert_eq!(r.status, StatusCode::NO_CONTENT);
    assert_eq!(h.audit_count("catalog.product.image.delete").await, 1);
    let r = h.call(&owner, Method::DELETE, &uri, None).await;
    assert_eq!(r.status, StatusCode::NO_CONTENT, "deleting nothing is fine");
    assert_eq!(h.audit_count("catalog.product.image.delete").await, 1);
    h.call(&owner, Method::GET, &uri, None)
        .await
        .assert_problem(404, "not_found");
}

async fn bad_uploads_are_rejected(h: &Harness) {
    let owner = h.owner_token().await;
    let doc = simple("FOTO2", "FOTO2");
    create(h, &owner, &doc).await;
    let uri = format!("/api/v1/products/{}/image", doc.id);
    let bearer = format!("Bearer {owner}");
    let auth = [("authorization", bearer.as_str())];

    let r = h
        .send_raw(Method::PUT, &uri, &auth, b"this is not an image".to_vec())
        .await;
    assert_eq!(r.status.as_u16(), 422);
    assert_eq!(r.headers["content-type"], "application/problem+json");
    assert_eq!(r.json()["code"], "invalid_image");
    // Wider than the 8000 px decode limit: refused before decoding.
    let wide = {
        let image = DynamicImage::ImageLuma8(image::GrayImage::new(8_001, 1));
        let mut out = Vec::new();
        image
            .write_to(&mut Cursor::new(&mut out), ImageFormat::Png)
            .unwrap();
        out
    };
    let r = h.send_raw(Method::PUT, &uri, &auth, wide).await;
    assert_eq!(
        (r.status.as_u16(), r.json()["code"].as_str()),
        (422, Some("invalid_image"))
    );
    let r = h
        .send_raw(Method::PUT, &uri, &auth, vec![0; 10 * 1024 * 1024 + 1])
        .await;
    assert_eq!(r.status.as_u16(), 413);
    assert_eq!(r.json()["code"], "payload_too_large");

    let unknown = format!("/api/v1/products/{}/image", new_id());
    let r = h.send_raw(Method::PUT, &unknown, &auth, png(10, 10)).await;
    assert_eq!(
        (r.status.as_u16(), r.json()["code"].as_str()),
        (404, Some("not_found"))
    );
    h.call(&owner, Method::DELETE, &unknown, None)
        .await
        .assert_problem(404, "not_found");
    h.call(&owner, Method::GET, &unknown, None)
        .await
        .assert_problem(404, "not_found");
    assert_eq!(h.audit_count("catalog.product.image.put").await, 0);
}
