//! Register lookup and search, branch assortment and `catalog.changed` events (spec 2.2 §6, §10,
//! §11, §14 server cases 18 and 22; phase C3).

use std::net::SocketAddr;
use std::time::Duration;

use axum::http::{Method, StatusCode};
use futures_util::StreamExt;
use serde_json::{Value, json};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

use crate::catalog_fixtures::{as_device, create, ean13, kaos, put_brand, put_product, simple};
use crate::common::{ANI_PIN, Harness, api_tests, new_id};

api_tests!(
    lookup_by_device_token_or_session,
    lookup_steps_and_errors,
    branch_blocks_drive_lookup_and_search,
    catalog_changed_reaches_connected_registers,
    search_pages,
);

/// Device token alone, a PIN session at a register, and back office with an optional
/// `branch_id` that must be one of the user's branches.
async fn lookup_by_device_token_or_session(h: &Harness) {
    let owner = h.owner_token().await;
    let doc = kaos("KAOS02", "899200");
    create(h, &owner, &doc).await;
    let code = ean13("899200000001");
    let uri = format!("/api/v1/catalog/lookup?code={code}");

    let r = as_device(h, &h.jkt_device, &uri).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert_eq!(
        r.body,
        json!({
            "product_id": doc.id,
            "variant_id": doc.variants[1],
            "unit_id": doc.base_unit,
            "name": "Kaos Polos – Small / Blue",
            "product_name": "Kaos Polos",
            "variant_label": "Small / Blue",
            "unit_name": "pcs",
            "sku": "KAOS02-S-BLUE",
            "plu": null,
            "type": "stock",
            "open_price": false,
            "requires_note": false,
            "source": "barcode",
            "matched_code": code,
            "qty_thousandths": 1000,
            "amount": null,
            "branch_id": h.jkt.id.to_string(),
            "unit_price": null,
        })
    );
    // The query cannot move a register to another branch.
    let r = as_device(h, &h.jkt_device, &format!("{uri}&branch_id={}", h.bdg.id)).await;
    assert_eq!(r.body["branch_id"], h.jkt.id.to_string());
    // A PIN session at the register: the register's branch.
    let cici = h.pin_token(&h.bdg_device, &h.cici, ANI_PIN).await;
    let r = h
        .at_register((&cici, &h.bdg_device), Method::GET, &uri, &[], None)
        .await;
    assert_eq!(r.body["branch_id"], h.bdg.id.to_string());
    // Back office: no assortment unless asked for, and only for an assigned branch.
    let r = h.call(&owner, Method::GET, &uri, None).await;
    assert_eq!(r.body["branch_id"], Value::Null);
    let r = h
        .call(
            &owner,
            Method::GET,
            &format!("{uri}&branch_id={}", h.bdg.id),
            None,
        )
        .await;
    assert_eq!(r.body["branch_id"], h.bdg.id.to_string());
    let budi = h.manager_token().await;
    let r = h
        .call(
            &budi,
            Method::GET,
            &format!("{uri}&branch_id={}", h.jkt.id),
            None,
        )
        .await;
    assert_eq!(r.body["branch_id"], h.jkt.id.to_string());
    h.call(
        &budi,
        Method::GET,
        &format!("{uri}&branch_id={}", h.bdg.id),
        None,
    )
    .await
    .assert_problem(403, "branch_not_assigned");
    h.call(&budi, Method::GET, &format!("{uri}&branch_id=x"), None)
        .await
        .assert_problem(400, "invalid_body");
    let r = h
        .call(
            &budi,
            Method::GET,
            &format!("/api/v1/catalog/search?q=kaos&branch_id={}", h.bdg.id),
            None,
        )
        .await;
    r.assert_problem(403, "branch_not_assigned");

    // No credentials, a bad device token, a bad session.
    h.send(Method::GET, &uri, &[], None)
        .await
        .assert_problem(401, "invalid_session");
    as_device(h, "nope", &uri)
        .await
        .assert_problem(401, "invalid_device_token");
    h.call("nope", Method::GET, &uri, None)
        .await
        .assert_problem(401, "invalid_session");
    // A PIN session without its device token is refused, as everywhere.
    h.call(&cici, Method::GET, &uri, None)
        .await
        .assert_problem(401, "invalid_device_token");
    h.call(&owner, Method::GET, "/api/v1/catalog/lookup", None)
        .await
        .assert_problem(400, "invalid_query");
}

async fn lookup_steps_and_errors(h: &Harness) {
    let owner = h.owner_token().await;
    let doc = kaos("KAOS03", "899300");
    create(h, &owner, &doc).await;
    let device = |uri: String| async move { as_device(h, &h.jkt_device, &uri).await };
    // Box barcode → the box unit; SKU in lower case → the default sale unit.
    let r = device("/api/v1/catalog/lookup?code=BOX-KAOS03".into()).await;
    assert_eq!(r.body["unit_id"], doc.body["units"][1]["id"]);
    assert_eq!(r.body["unit_name"], "box");
    let r = device("/api/v1/catalog/lookup?code=kaos03-m-red".into()).await;
    assert_eq!(
        (r.body["source"].as_str(), r.body["sku"].as_str()),
        (Some("sku"), Some("KAOS03-M-RED"))
    );
    assert_eq!(r.body["matched_code"], "KAOS03-M-RED");
    assert_eq!(r.body["variant_label"], "Medium / Red");

    // A weighed product: PLU typed alone, and a weight scale barcode 21 00123 01250 C.
    let (kg, v) = (new_id(), new_id());
    let cheese = json!({
        "code": "KEJU", "name": "Keju", "type": "weighed",
        "tax_class_id": crate::catalog_fixtures::STANDARD,
        "units": [{ "id": kg, "name": "kg", "kind": "decimal", "factor_thousandths": 1000 }],
        "base_unit_id": kg, "variants": [{ "id": v, "plu": 123 }],
    });
    assert_eq!(
        put_product(h, &owner, &new_id(), &cheese)
            .await
            .status
            .as_u16(),
        201
    );
    let r = device("/api/v1/catalog/lookup?code=123".into()).await;
    assert_eq!(
        (r.body["source"].as_str(), r.body["variant_id"].as_str()),
        (Some("plu"), Some(v.as_str()))
    );
    let scale = ean13("210012301250");
    let r = device(format!("/api/v1/catalog/lookup?code={scale}")).await;
    assert_eq!(r.body["source"], "scale_barcode");
    assert_eq!(r.body["qty_thousandths"], 1250);
    assert_eq!(
        (
            &r.body["plu"],
            &r.body["unit_name"],
            &r.body["matched_code"]
        ),
        (&json!(123), &json!("kg"), &json!(scale))
    );
    assert_eq!(r.body["variant_label"], "");
    let price = ean13("220012315000");
    let r = device(format!("/api/v1/catalog/lookup?code={price}")).await;
    assert_eq!(r.body["amount"], 15_000);
    assert_eq!(r.body["qty_thousandths"], Value::Null);
    let zero = ean13("210012300000");
    device(format!("/api/v1/catalog/lookup?code={zero}"))
        .await
        .assert_problem(422, "scale_value_zero");

    // Not found (with `errors`), empty code, not sellable.
    let r = device("/api/v1/catalog/lookup?code=NOPE".into()).await;
    r.assert_problem(404, "not_found");
    assert_eq!(r.body["errors"][0]["code"], "not_found");
    device("/api/v1/catalog/lookup?code=%20".into())
        .await
        .assert_problem(404, "not_found");
    let mut unsellable = simple("NOSALE", "NOSALE-1");
    unsellable.body["sellable"] = false.into();
    create(h, &owner, &unsellable).await;
    device("/api/v1/catalog/lookup?code=NOSALE-1".into())
        .await
        .assert_problem(422, "not_sellable");

    // Search: every sellable variant, prefix matches first; `limit` 1–50.
    let r = device("/api/v1/catalog/search?q=kaos".into()).await;
    let items = r.body["items"].as_array().unwrap();
    assert_eq!(items.len(), 4);
    assert_eq!(items[0]["name"], "Kaos Polos – Small / Red");
    assert_eq!(items[0]["unit_id"], doc.base_unit);
    assert_eq!(r.body["branch_id"], h.jkt.id.to_string());
    let r = device("/api/v1/catalog/search?q=kaos&limit=2".into()).await;
    assert_eq!(r.body["items"].as_array().unwrap().len(), 2);
    let r = device("/api/v1/catalog/search?q=".into()).await;
    assert_eq!(r.body["items"], json!([]));
    device("/api/v1/catalog/search?q=x&limit=0".into())
        .await
        .assert_problem(422, "invalid_field");
    device("/api/v1/catalog/search?q=x&limit=51".into())
        .await
        .assert_problem(422, "invalid_field");
    let r = device("/api/v1/catalog/search?q=nosale".into()).await;
    assert_eq!(r.body["items"], json!([]), "not sellable is hidden");
}

/// Paging of the search (§6, C3b): full pages carry `next_after_variant_id`; the pages add up to
/// the whole result.
async fn search_pages(h: &Harness) {
    let owner = h.owner_token().await;
    create(h, &owner, &kaos("KAOS09", "899209")).await;
    let device = |uri: String| async move { as_device(h, &h.jkt_device, &uri).await };
    let all = device("/api/v1/catalog/search?q=kaos".into()).await;
    let items = all.body["items"].as_array().unwrap().clone();
    assert_eq!(items.len(), 4);
    assert_eq!(all.body["next_after_variant_id"], Value::Null, "not full");
    let r = device("/api/v1/catalog/search?q=kaos&limit=2".into()).await;
    let cursor = r.body["next_after_variant_id"].as_str().unwrap().to_owned();
    assert_eq!(r.body["items"][1]["variant_id"], cursor.as_str());
    let next = device(format!(
        "/api/v1/catalog/search?q=kaos&limit=2&after_variant_id={cursor}"
    ))
    .await;
    assert_eq!(next.body["items"][0], items[2]);
    assert_eq!(next.body["items"][1], items[3]);
    let last = next.body["next_after_variant_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let end = device(format!(
        "/api/v1/catalog/search?q=kaos&limit=2&after_variant_id={last}"
    ))
    .await;
    assert_eq!(end.body["items"], json!([]));
    assert_eq!(end.body["next_after_variant_id"], Value::Null);
    device("/api/v1/catalog/search?q=kaos&after_variant_id=nope".into())
        .await
        .assert_problem(400, "invalid_body");
}

/// §14 item 18.
async fn branch_blocks_drive_lookup_and_search(h: &Harness) {
    let owner = h.owner_token().await;
    let doc = simple("ROKOK", "ROKOK-1");
    create(h, &owner, &doc).await;
    let blocks = format!("/api/v1/products/{}/branch-blocks", doc.id);
    let r = h.call(&owner, Method::GET, &blocks, None).await;
    assert_eq!(r.body, json!({ "branch_ids": [] }));
    let r = h
        .call(
            &owner,
            Method::PUT,
            &blocks,
            Some(json!({ "branch_ids": [h.jkt.id.to_string(), h.jkt.id.to_string()] })),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert_eq!(r.body, json!({ "branch_ids": [h.jkt.id.to_string()] }));
    assert_eq!(h.audit_count("catalog.product.branch_blocks.put").await, 1);
    // Replay: unchanged.
    h.call(
        &owner,
        Method::PUT,
        &blocks,
        Some(json!({ "branch_ids": [h.jkt.id.to_string()] })),
    )
    .await;
    assert_eq!(h.audit_count("catalog.product.branch_blocks.put").await, 1);
    let r = as_device(h, &h.bdg_device, &blocks).await;
    r.assert_problem(401, "invalid_session");

    let lookup = "/api/v1/catalog/lookup?code=ROKOK-1";
    as_device(h, &h.jkt_device, lookup)
        .await
        .assert_problem(422, "not_sold_here");
    let r = as_device(h, &h.bdg_device, lookup).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    // Back office: without a branch nothing is blocked; with JKT01 it is.
    assert_eq!(
        h.call(&owner, Method::GET, lookup, None).await.status,
        StatusCode::OK
    );
    h.call(
        &owner,
        Method::GET,
        &format!("{lookup}&branch_id={}", h.jkt.id),
        None,
    )
    .await
    .assert_problem(422, "not_sold_here");
    let search = "/api/v1/catalog/search?q=rokok";
    let r = as_device(h, &h.jkt_device, search).await;
    assert_eq!(r.body["items"], json!([]));
    let r = as_device(h, &h.bdg_device, search).await;
    assert_eq!(r.body["items"].as_array().unwrap().len(), 1);

    for (body, status, code) in [
        (json!({ "branch_ids": [new_id()] }), 422, "invalid_field"),
        (json!({ "branch_ids": ["x"] }), 400, "invalid_body"),
    ] {
        h.call(&owner, Method::PUT, &blocks, Some(body))
            .await
            .assert_problem(status, code);
    }
    let unknown = format!("/api/v1/products/{}/branch-blocks", new_id());
    h.call(
        &owner,
        Method::PUT,
        &unknown,
        Some(json!({ "branch_ids": [] })),
    )
    .await
    .assert_problem(404, "not_found");
    h.call(&owner, Method::GET, &unknown, None)
        .await
        .assert_problem(404, "not_found");
}

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

async fn serve(h: &Harness) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = h.app();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    addr
}

async fn connect(addr: SocketAddr, token: &str, device: &str) -> Ws {
    let mut request = format!("ws://{addr}/api/v1/events")
        .into_client_request()
        .unwrap();
    let headers = request.headers_mut();
    headers.insert("authorization", format!("Bearer {token}").parse().unwrap());
    headers.insert("x-register-token", device.parse().unwrap());
    connect_async(request).await.unwrap().0
}

async fn next(ws: &mut Ws) -> Value {
    loop {
        let message = tokio::time::timeout(Duration::from_secs(5), ws.next())
            .await
            .expect("no event within 5 s");
        if let Some(Ok(Message::Text(text))) = message {
            return serde_json::from_str(&text).unwrap();
        }
    }
}

/// §14 item 22: every register gets `catalog.changed` (the catalog is company-wide); a replay
/// that changed nothing sends no event.
async fn catalog_changed_reaches_connected_registers(h: &Harness) {
    let addr = serve(h).await;
    let owner = h.owner_token().await;
    let ani = h.pin_token(&h.jkt_device, &h.ani, ANI_PIN).await;
    let cici = h.pin_token(&h.bdg_device, &h.cici, ANI_PIN).await;
    let mut jkt = connect(addr, &ani, &h.jkt_device).await;
    let mut bdg = connect(addr, &cici, &h.bdg_device).await;

    let doc = simple("EVT1", "EVT-1");
    create(h, &owner, &doc).await;
    let changed = json!({
        "type": "catalog.changed",
        "data": { "scope": "product", "product_id": doc.id },
    });
    assert_eq!(next(&mut jkt).await, changed);
    assert_eq!(next(&mut bdg).await, changed);

    // The replay sends nothing: the next event is the brand's.
    put_product(h, &owner, &doc.id, &doc.body).await;
    let brand = new_id();
    put_brand(h, &owner, &brand, "Sampoerna").await;
    let event = next(&mut bdg).await;
    assert_eq!(
        event,
        json!({ "type": "catalog.changed", "data": { "scope": "brand", "brand_id": brand } })
    );
    assert_eq!(next(&mut jkt).await, event);

    let category = new_id();
    crate::catalog_fixtures::put_category(h, &owner, &category, json!({ "name": "Rokok" })).await;
    assert_eq!(
        next(&mut jkt).await["data"],
        json!({ "scope": "category", "category_id": category })
    );
    let settings = h
        .call(&owner, Method::GET, "/api/v1/settings/catalog", None)
        .await;
    let mut body = settings.body;
    body["unit_names"] = json!(["pcs", "slop"]);
    h.call(&owner, Method::PUT, "/api/v1/settings/catalog", Some(body))
        .await;
    assert_eq!(next(&mut jkt).await["data"], json!({ "scope": "settings" }));
    h.call(
        &owner,
        Method::POST,
        &format!("/api/v1/products/{}/archive", doc.id),
        None,
    )
    .await;
    assert_eq!(next(&mut jkt).await["data"]["product_id"], doc.id);
    // The BDG01 register got every one of them as well.
    for scope in ["category", "settings", "product"] {
        assert_eq!(next(&mut bdg).await["data"]["scope"], scope);
    }
}
