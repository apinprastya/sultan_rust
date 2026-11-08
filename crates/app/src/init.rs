//! `sultan init`: first-run setup of a node's database (plan Phase 7, spec 2.1 §1, §3, §4).
//!
//! In one go it:
//! 1. connects (`SQLite` files are created; a `MySQL` database must exist) and runs the
//!    migrations;
//! 2. refuses if a company already exists, so it can never overwrite a running shop;
//! 3. seeds the default roles (`sultan_db::seed`);
//! 4. in **one transaction** creates the company (exactly one per install, spec 2.1 §1), the first
//!    branch, its default receiving stock location (spec 2.1 §1: "one is the default receiving
//!    location") and the Owner account with password + PIN, assigned the Owner role for all
//!    branches (spec 2.1 §3 invariant), plus a `system.init` audit entry.
//!
//! Inputs are checked with the same rules as the API before anything is written: branch code
//! 1–10 letters/digits, names 1–64/1–128 characters, an IANA time zone (`--timezone`, default
//! `Asia/Jakarta`; document numbers use the branch-local date), PIN 4–6 digits, password ≥ 8
//! characters.
//!
//! A `SQLite` file is created readable and writable by its owner only (`0600`, with its `-wal` /
//! `-shm` / `-journal` side files) on Unix; `sultan serve` keeps it so (`sultan_db::Db::connect`).
//! The repositories append the usual outbox rows, so an HQ's first branch syncs like any other.

use serde_json::json;
use sultan_core::access::BranchScope;
use sultan_core::ids::{BranchId, CompanyId, LocationId, UserId};
use sultan_core::time::Timestamp;
use sultan_db::audit::{self, AuditEntry};
use sultan_db::org::{Branch, Company, StockLocation};
use sultan_db::users::User;
use sultan_db::{Db, org, roles, seed, users};
use sultan_server::{ApiError, credentials, routes};

use crate::error::{AppError, Result};

/// Everything `sultan init` creates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InitOptions {
    /// Database URL.
    pub db_url: String,
    /// Company name.
    pub company: String,
    /// First branch code, e.g. `JKT01`.
    pub branch_code: String,
    /// First branch name.
    pub branch_name: String,
    /// First branch address (may be empty).
    pub branch_address: String,
    /// First branch IANA time zone, e.g. `Asia/Jakarta`.
    pub timezone: String,
    /// Name of the branch's default receiving stock location.
    pub location_name: String,
    /// Owner username.
    pub owner_username: String,
    /// Owner display name (lock screen).
    pub owner_display_name: String,
    /// Owner password (plain; hashed here).
    pub owner_password: String,
    /// Owner PIN (plain; hashed here).
    pub owner_pin: String,
}

/// What `sultan init` created.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InitReport {
    /// The company.
    pub company: Company,
    /// The first branch.
    pub branch: Branch,
    /// Its default receiving location.
    pub location: StockLocation,
    /// The Owner's user ID.
    pub owner_id: UserId,
}

fn invalid(error: &ApiError) -> AppError {
    AppError::Invalid(error.detail().to_owned())
}

/// Builds the rows from the options, checking every rule before the database is touched.
fn build(options: &InitOptions) -> Result<(Company, Branch, StockLocation, User)> {
    let company = Company {
        id: CompanyId::generate(),
        name: routes::text("company", &options.company, 128).map_err(|e| invalid(&e))?,
    };
    let branch = Branch {
        id: BranchId::generate(),
        company: company.id,
        code: routes::code("branch", &options.branch_code).map_err(|e| invalid(&e))?,
        name: routes::text("branch-name", &options.branch_name, 128).map_err(|e| invalid(&e))?,
        address: options.branch_address.trim().to_owned(),
        npwp: None,
        receipt_header: String::new(),
        receipt_footer: String::new(),
        timezone: routes::timezone("timezone", &options.timezone).map_err(|e| invalid(&e))?,
        archived: false,
    };
    let location = StockLocation {
        id: LocationId::generate(),
        branch: branch.id,
        name: routes::text("location-name", &options.location_name, 64).map_err(|e| invalid(&e))?,
        receiving_default: true,
        archived: false,
    };
    let owner = User {
        id: UserId::generate(),
        username: routes::text("owner", &options.owner_username, 64).map_err(|e| invalid(&e))?,
        display_name: routes::text("owner-name", &options.owner_display_name, 64)
            .map_err(|e| invalid(&e))?,
        password_hash: Some(
            credentials::hash_password(&options.owner_password).map_err(|e| invalid(&e))?,
        ),
        pin_hash: Some(credentials::hash_pin(&options.owner_pin).map_err(|e| invalid(&e))?),
        active: true,
    };
    Ok((company, branch, location, owner))
}

/// Sets up a new database (see the module docs).
///
/// # Errors
/// [`AppError::Invalid`] for a value that breaks a rule, [`AppError::AlreadyInitialised`] when a
/// company exists, or [`AppError::Db`].
pub async fn init_database(options: &InitOptions, now: Timestamp) -> Result<InitReport> {
    let (company, branch, location, owner) = build(options)?;
    let db = Db::connect(&options.db_url).await?;
    let result = create(&db, &company, &branch, &location, &owner, now).await;
    db.close().await;
    result?;
    Ok(InitReport {
        company,
        branch,
        location,
        owner_id: owner.id,
    })
}

async fn create(
    db: &Db,
    company: &Company,
    branch: &Branch,
    location: &StockLocation,
    owner: &User,
    now: Timestamp,
) -> Result<()> {
    db.migrate().await?;
    if let Some(existing) = org::get_company(db).await? {
        return Err(AppError::AlreadyInitialised(existing.name));
    }
    seed::seed_defaults(db, now).await?;
    let mut tx = db.begin().await?;
    org::save_company(&mut tx, company, now).await?;
    org::save_branch(&mut tx, branch, now).await?;
    org::save_location(&mut tx, location, now).await?;
    users::save_user(&mut tx, owner, now).await?;
    roles::replace_assignments(
        &mut tx,
        owner.id,
        &[(seed::OWNER_ROLE, BranchScope::All)],
        now,
    )
    .await?;
    let mut entry = AuditEntry::new(now, "system.init");
    entry.branch = Some(branch.id);
    entry.user = Some(owner.id);
    entry.target = Some(format!("company:{}", company.id));
    entry.after = Some(json!({
        "company": company.name,
        "branch_code": branch.code,
        "location": location.name,
        "owner": owner.username,
    }));
    audit::append(&mut tx, &entry).await?;
    tx.commit().await?;
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use std::path::PathBuf;

    use super::*;

    pub fn options(db_url: &str) -> InitOptions {
        InitOptions {
            db_url: db_url.to_owned(),
            company: "PT Sultan Demo".into(),
            branch_code: "JKT01".into(),
            branch_name: "Toko Jakarta".into(),
            branch_address: " Jl. Demo 1 ".into(),
            timezone: "Asia/Jakarta".into(),
            location_name: "Store floor".into(),
            owner_username: "owner".into(),
            owner_display_name: "Owner".into(),
            owner_password: "owner-pass-1".into(),
            owner_pin: "1234".into(),
        }
    }

    pub struct TempDir(pub PathBuf);

    impl TempDir {
        pub fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("sultan-app-{name}-{}", uuid_like()));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        pub fn sqlite_url(&self) -> String {
            format!("sqlite://{}", self.0.join("sultan.db").display())
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn uuid_like() -> String {
        UserId::generate().to_string()
    }

    const NOW: Timestamp = Timestamp::from_unix_millis(1_800_000_000_000);

    #[tokio::test]
    async fn creates_company_branch_location_and_owner_once() {
        let dir = TempDir::new("init");
        let url = dir.sqlite_url();
        let report = init_database(&options(&url), NOW).await.unwrap();
        assert_eq!(report.branch.code, "JKT01");
        assert_eq!(report.branch.address, "Jl. Demo 1");
        assert_eq!(report.branch.timezone, "Asia/Jakarta");
        assert!(report.location.receiving_default);

        let db = Db::connect(&url).await.unwrap();
        assert_eq!(
            org::get_company(&db).await.unwrap(),
            Some(report.company.clone())
        );
        let branch = org::find_branch_by_code(&db, "JKT01")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(branch, report.branch);
        let locations = org::list_locations(&db, Some(branch.id)).await.unwrap();
        assert_eq!(locations, std::slice::from_ref(&report.location));
        let owner = users::find_by_username(&db, "owner")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(owner.id, report.owner_id);
        assert!(credentials::verify_secret(
            owner.password_hash.as_deref().unwrap(),
            "owner-pass-1"
        ));
        assert!(credentials::verify_secret(
            owner.pin_hash.as_deref().unwrap(),
            "1234"
        ));
        let assignments = roles::assignments_for_user(&db, owner.id).await.unwrap();
        assert_eq!(assignments.len(), 1);
        db.close().await;

        let mut again = options(&url);
        again.company = "Other".into();
        let err = init_database(&again, NOW).await.unwrap_err();
        assert!(
            matches!(&err, AppError::AlreadyInitialised(name) if name == "PT Sultan Demo"),
            "{err}"
        );
    }

    /// Phase 9: the new database file and its WAL side files are owner-only.
    #[cfg(unix)]
    #[tokio::test]
    async fn database_files_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new("init-perm");
        let mut o = options(&dir.sqlite_url());
        o.timezone = " Asia/Makassar ".into();
        let report = init_database(&o, NOW).await.unwrap();
        assert_eq!(report.branch.timezone, "Asia/Makassar");
        let names: Vec<String> = std::fs::read_dir(&dir.0)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert!(names.contains(&"sultan.db".to_owned()), "{names:?}");
        for name in names {
            let mode = std::fs::metadata(dir.0.join(&name))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600, "{name}");
        }
    }

    #[tokio::test]
    async fn invalid_values_are_rejected_before_touching_the_database() {
        type Edit = fn(&mut InitOptions);
        let cases: [(Edit, &str); 9] = [
            (|o| o.owner_pin = "12".into(), "a PIN is 4 to 6 digits"),
            (|o| o.owner_pin = "12ab".into(), "a PIN is 4 to 6 digits"),
            (
                |o| o.owner_password = "short".into(),
                "a password has at least 8 characters",
            ),
            (
                |o| o.branch_code = "JKT-01".into(),
                "`branch` must be 1 to 10 letters or digits",
            ),
            (
                |o| o.company = "  ".into(),
                "`company` must be 1 to 128 characters",
            ),
            (
                |o| o.branch_name = String::new(),
                "`branch-name` must be 1 to 128 characters",
            ),
            (
                |o| o.owner_username = String::new(),
                "`owner` must be 1 to 64 characters",
            ),
            (
                |o| o.location_name = String::new(),
                "`location-name` must be 1 to 64 characters",
            ),
            (
                |o| o.timezone = "WIB".into(),
                "`timezone`: `WIB` is not an IANA time zone, e.g. Asia/Jakarta",
            ),
        ];
        for (edit, message) in cases {
            // An unsupported URL proves the database is never reached.
            let mut o = options("postgres://never");
            edit(&mut o);
            let err = init_database(&o, NOW).await.unwrap_err();
            assert_eq!(err.to_string(), message);
        }
        let err = init_database(&options("postgres://x"), NOW)
            .await
            .unwrap_err();
        assert!(matches!(err, AppError::Db(_)), "{err}");
    }
}
