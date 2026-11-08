//! The events WebSocket over a real TCP listener (spec 2.1 §4, §9 case 18; plan Phase 6).

use std::net::SocketAddr;
use std::time::Duration;

use axum::http::{Method, StatusCode};
use futures_util::StreamExt;
use serde_json::{Value, json};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::{Error as WsError, Message};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

use crate::common::{ANI_PIN, BUDI_PIN, Harness, api_tests};

api_tests!(
    unauthenticated_upgrade_is_rejected,
    session_revoked_after_logout_from_another_session,
    branch_scoped_events,
    user_changed_and_deactivation,
);

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Serves the harness's app (sharing its event hub) on a free local port.
async fn serve(h: &Harness) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = h.app();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    addr
}

async fn connect(addr: SocketAddr, headers: &[(&str, &str)]) -> Result<Ws, WsError> {
    let mut request = format!("ws://{addr}/api/v1/events")
        .into_client_request()
        .unwrap();
    for (name, value) in headers {
        let name: axum::http::HeaderName = name.parse().unwrap();
        request.headers_mut().insert(name, value.parse().unwrap());
    }
    connect_async(request).await.map(|(ws, _)| ws)
}

async fn connect_as(addr: SocketAddr, token: &str, device: Option<&str>) -> Ws {
    let bearer = format!("Bearer {token}");
    let mut headers = vec![("authorization", bearer.as_str())];
    if let Some(d) = device {
        headers.push(("x-register-token", d));
    }
    connect(addr, &headers).await.unwrap()
}

/// The next event, or `None` when the server closed the socket.
async fn next(ws: &mut Ws) -> Option<Value> {
    loop {
        let message = tokio::time::timeout(Duration::from_secs(5), ws.next())
            .await
            .expect("no event within 5 s");
        match message {
            Some(Ok(Message::Text(text))) => return Some(serde_json::from_str(&text).unwrap()),
            Some(Ok(Message::Close(_)) | Err(_)) | None => return None,
            Some(Ok(_)) => {}
        }
    }
}

fn status_of(error: WsError) -> u16 {
    match error {
        WsError::Http(response) => response.status().as_u16(),
        other => panic!("expected an HTTP rejection, got {other}"),
    }
}

async fn unauthenticated_upgrade_is_rejected(h: &Harness) {
    let addr = serve(h).await;
    assert_eq!(status_of(connect(addr, &[]).await.unwrap_err()), 401);
    assert_eq!(
        status_of(
            connect(addr, &[("authorization", "Bearer nope")])
                .await
                .unwrap_err()
        ),
        401
    );
    // A PIN session needs its register's device token too.
    let ani = h.pin_token(&h.jkt_device, &h.ani, ANI_PIN).await;
    let bearer = format!("Bearer {ani}");
    assert_eq!(
        status_of(
            connect(addr, &[("authorization", &bearer)])
                .await
                .unwrap_err()
        ),
        401
    );
    // A plain GET without upgrade headers is refused as well.
    let r = h.send(Method::GET, "/api/v1/events", &[], None).await;
    r.assert_problem(401, "invalid_session");

    // Browsers pass the tokens as subprotocols; the server selects `sultan.events.v1`.
    let protocols = format!("sultan.events.v1, bearer.{ani}, register.{}", h.jkt_device);
    let mut request = format!("ws://{addr}/api/v1/events")
        .into_client_request()
        .unwrap();
    request
        .headers_mut()
        .insert("sec-websocket-protocol", protocols.parse().unwrap());
    let (_ws, response) = connect_async(request).await.unwrap();
    assert_eq!(
        response.headers()["sec-websocket-protocol"],
        "sultan.events.v1"
    );
}

/// Spec 2.1 §9 case 18.
async fn session_revoked_after_logout_from_another_session(h: &Harness) {
    let addr = serve(h).await;
    // Budi at the register (session 1, with the socket) and in back office (session 2).
    let at_register = h.pin_token(&h.jkt_device, &h.budi, BUDI_PIN).await;
    let back_office = h.manager_token().await;
    let mut ws = connect_as(addr, &at_register, Some(&h.jkt_device)).await;

    assert_eq!(
        h.logout(&back_office, None).await.status,
        StatusCode::NO_CONTENT
    );
    let event = next(&mut ws).await.unwrap();
    assert_eq!(
        event,
        json!({
            "type": "session.revoked",
            "data": { "user_id": h.budi.id.to_string(), "reason": "logout", "this_session": false },
        })
    );

    // Another user's logout is not sent to Budi.
    let ani = h.pin_token(&h.jkt_device, &h.ani, ANI_PIN).await;
    h.logout(&ani, Some(&h.jkt_device)).await;

    // Logging out the socket's own session: the event, then the socket is closed.
    h.logout(&at_register, Some(&h.jkt_device)).await;
    let event = next(&mut ws).await.unwrap();
    assert_eq!(event["type"], "session.revoked");
    assert_eq!(event["data"]["this_session"], true);
    assert_eq!(next(&mut ws).await, None);
}

async fn branch_scoped_events(h: &Harness) {
    let addr = serve(h).await;
    let owner = h.owner_token().await;
    let cici = h.pin_token(&h.bdg_device, &h.cici, ANI_PIN).await;
    let ani = h.pin_token(&h.jkt_device, &h.ani, ANI_PIN).await;
    let mut owner_ws = connect_as(addr, &owner, None).await;
    let mut cici_ws = connect_as(addr, &cici, Some(&h.bdg_device)).await;

    let open = |token: String, device: String| async move {
        h.at_register(
            (&token, &device),
            Method::POST,
            "/api/v1/cashier-sessions",
            &[],
            Some(json!({ "opening_cash": 0 })),
        )
        .await
    };
    let jkt = open(ani.clone(), h.jkt_device.clone()).await;
    let jkt_id = jkt.body["id"].as_str().unwrap().to_owned();
    let event = next(&mut owner_ws).await.unwrap();
    assert_eq!(
        event,
        json!({
            "type": "cashier_session.opened",
            "data": {
                "session_id": jkt_id,
                "number": jkt.body["number"],
                "register_id": h.jkt_register.id.to_string(),
                "branch_id": h.jkt.id.to_string(),
                "user_id": h.ani.id.to_string(),
            },
        })
    );

    // Cici (Cashier at BDG01 only) does not see JKT01: her first event is her own branch's.
    open(cici.clone(), h.bdg_device.clone()).await;
    let event = next(&mut cici_ws).await.unwrap();
    assert_eq!(event["type"], "cashier_session.opened");
    assert_eq!(event["data"]["branch_id"], h.bdg.id.to_string());
    // The Owner (all branches) sees both.
    let event = next(&mut owner_ws).await.unwrap();
    assert_eq!(event["data"]["branch_id"], h.bdg.id.to_string());

    let closed = h
        .at_register(
            (&ani, &h.jkt_device),
            Method::POST,
            &format!("/api/v1/cashier-sessions/{jkt_id}/close"),
            &[],
            Some(json!({ "counted_cash": 0 })),
        )
        .await;
    assert_eq!(closed.status, StatusCode::OK);
    let event = next(&mut owner_ws).await.unwrap();
    assert_eq!(event["type"], "cashier_session.closed");
    assert_eq!(event["data"]["session_id"], jkt_id);
    assert_eq!(event["data"]["forced"], false);
}

async fn user_changed_and_deactivation(h: &Harness) {
    let addr = serve(h).await;
    let owner = h.owner_token().await;
    let ani = h.pin_token(&h.jkt_device, &h.ani, ANI_PIN).await;
    let cici = h.pin_token(&h.bdg_device, &h.cici, ANI_PIN).await;
    let budi = h.pin_token(&h.jkt_device, &h.budi, BUDI_PIN).await;
    let mut ani_ws = connect_as(addr, &ani, Some(&h.jkt_device)).await;
    let mut cici_ws = connect_as(addr, &cici, Some(&h.bdg_device)).await;
    let mut budi_ws = connect_as(addr, &budi, Some(&h.jkt_device)).await;

    let uri = format!("/api/v1/users/{}", h.ani.id);
    let r = h
        .call(
            &owner,
            Method::PUT,
            &uri,
            Some(json!({ "username": "ani", "display_name": "Ani Lestari" })),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    let changed = json!({ "type": "user.changed", "data": { "user_id": h.ani.id.to_string() } });
    // The user themselves, and a colleague at the same branch (the lock screen list changed).
    assert_eq!(next(&mut ani_ws).await.unwrap(), changed);
    assert_eq!(next(&mut budi_ws).await.unwrap(), changed);

    // Deactivation revokes Ani's sessions: her socket is told and closed.
    let r = h.call(&owner, Method::DELETE, &uri, None).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    let event = next(&mut ani_ws).await.unwrap();
    assert_eq!(
        event,
        json!({
            "type": "session.revoked",
            "data": {
                "user_id": h.ani.id.to_string(),
                "reason": "user_deactivated",
                "this_session": true,
            },
        })
    );
    assert_eq!(next(&mut ani_ws).await, None);
    h.at_register(
        (&ani, &h.jkt_device),
        Method::GET,
        "/api/v1/cashier-sessions/current",
        &[],
        None,
    )
    .await
    .assert_problem(401, "invalid_session");
    assert_eq!(next(&mut budi_ws).await.unwrap(), changed);

    // Cici (BDG01) saw neither change: her first event is from her own branch.
    h.at_register(
        (&cici, &h.bdg_device),
        Method::POST,
        "/api/v1/cashier-sessions",
        &[],
        Some(json!({ "opening_cash": 0 })),
    )
    .await;
    let event = next(&mut cici_ws).await.unwrap();
    assert_eq!(event["type"], "cashier_session.opened");

    // A socket whose session has expired is closed at the next event it would get.
    h.clock.advance(crate::common::IDLE_MS + 1);
    h.at_register(
        (&cici, &h.bdg_device),
        Method::GET,
        "/api/v1/cashier-sessions/current",
        &[],
        None,
    )
    .await
    .assert_problem(401, "invalid_session");
    h.events.publish(sultan_server::events::Event::for_branch(
        "cashier_session.opened",
        h.bdg.id,
        json!({}),
    ));
    assert_eq!(next(&mut cici_ws).await, None);
}
