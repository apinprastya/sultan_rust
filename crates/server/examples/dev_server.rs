//! Development server for manual and Postman testing on a throwaway in-memory database. Not for
//! production: the real server is `sultan init` + `sultan serve` (`crates/app`, plan Phase 7; see
//! `postman/README.md`). This example stays in the server crate because it needs no files and
//! `sultan-app` depends on this crate, not the other way round.
//!
//! Starts the real router as a `standalone` node on `127.0.0.1:<port>` (default 8080, or the
//! first argument) with a fresh in-memory `SQLite` database seeded with:
//! - the company "PT Sultan Demo" and branch `JKT01`;
//! - the default roles;
//! - user `owner` (Owner for all branches), password `owner-pass-1`, PIN `1234`. The PIN lets the
//!   owner approve manager overrides (`POST /overrides`) at any register.
//!
//! Event notifications are served at `ws://127.0.0.1:<port>/api/v1/events` (bearer token, plus
//! `X-Register-Token` for PIN sessions; see `sultan_server::events`).
//!
//! ```sh
//! cargo run -p sultan-server --example dev_server -- 8080
//! postman collection run postman/sultan-api.postman_collection.json \
//!     --env-var baseUrl=http://127.0.0.1:8080/api/v1
//! ```
//!
//! Everything is lost when the process stops. It writes no database file; if it is ever pointed
//! at a `SQLite` file, `Db::connect` keeps that file and its side files owner-only (`0600`, Phase 9)
//! as for `sultan init` / `sultan serve`.

use std::sync::Arc;

use sultan_core::access::BranchScope;
use sultan_core::ids::{BranchId, CompanyId, UserId};
use sultan_db::org::{Branch, Company};
use sultan_db::users::User;
use sultan_db::{Db, org, roles, seed, users};
use sultan_server::{AppState, Clock, NodeRole, SystemClock, credentials, router};

/// Demo Owner credentials (also in `postman/README.md`).
const OWNER_USERNAME: &str = "owner";
const OWNER_PASSWORD: &str = "owner-pass-1";
const OWNER_PIN: &str = "1234";

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let port: u16 = std::env::args().nth(1).map_or(Ok(8080), |p| p.parse())?;
    let db = Db::connect("sqlite::memory:").await?;
    db.migrate().await?;
    let now = SystemClock.now();
    seed::seed_defaults(&db, now).await?;

    let company = Company {
        id: CompanyId::generate(),
        name: "PT Sultan Demo".to_owned(),
    };
    let branch = Branch {
        id: BranchId::generate(),
        company: company.id,
        code: "JKT01".to_owned(),
        name: "Toko Jakarta".to_owned(),
        address: "Jl. Demo 1, Jakarta".to_owned(),
        npwp: None,
        receipt_header: "Terima kasih".to_owned(),
        receipt_footer: String::new(),
        timezone: "Asia/Jakarta".to_owned(),
        archived: false,
    };
    let owner = User {
        id: UserId::generate(),
        username: OWNER_USERNAME.to_owned(),
        display_name: "Owner".to_owned(),
        pin_hash: Some(credentials::hash_secret(OWNER_PIN)),
        password_hash: Some(credentials::hash_secret(OWNER_PASSWORD)),
        active: true,
    };
    let mut tx = db.begin().await?;
    org::save_company(&mut tx, &company, now).await?;
    org::save_branch(&mut tx, &branch, now).await?;
    users::save_user(&mut tx, &owner, now).await?;
    roles::replace_assignments(
        &mut tx,
        owner.id,
        &[(seed::OWNER_ROLE, BranchScope::All)],
        now,
    )
    .await?;
    tx.commit().await?;

    let state = AppState::new(db, NodeRole::Standalone, Arc::new(SystemClock));
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
    let addr = listener.local_addr()?;
    println!(
        "sultan dev server on http://{addr}/api/v1, events at ws://{addr}/api/v1/events \
         (branch JKT01 {}, owner {OWNER_USERNAME} / {OWNER_PASSWORD} / PIN {OWNER_PIN}, user id {})",
        branch.id, owner.id
    );
    axum::serve(listener, router(state))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}
