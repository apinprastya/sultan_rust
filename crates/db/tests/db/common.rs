//! Test harness: one test body, run on in-memory `SQLite` and (when `SULTAN_TEST_MYSQL_URL` is set)
//! on a fresh `MySQL` database per test.

use std::str::FromStr;

use sqlx::mysql::{MySqlConnectOptions, MySqlPool, MySqlPoolOptions};
use sultan_core::ids::{BranchId, CompanyId, LocationId, RegisterId, UserId};
use sultan_core::time::Timestamp;
use sultan_db::org::{Branch, Company, Register, StockLocation};
use sultan_db::users::User;
use sultan_db::{Db, TokenHash};

/// Generates `sqlite::<name>` and `mysql::<name>` tests for each `async fn <name>(db: &Db)`.
macro_rules! db_tests {
    ($($name:ident),* $(,)?) => {
        mod sqlite {
            $(
                #[tokio::test]
                async fn $name() {
                    let db = crate::common::sqlite().await;
                    super::$name(&db).await;
                }
            )*
        }
        mod mysql {
            $(
                #[tokio::test]
                async fn $name() {
                    if let Some(test_db) = crate::common::MySqlTestDb::create().await {
                        super::$name(&test_db.db).await;
                        test_db.drop().await;
                    }
                }
            )*
        }
    };
}
pub(crate) use db_tests;

/// A migrated in-memory `SQLite` database.
pub async fn sqlite() -> Db {
    let db = Db::connect("sqlite::memory:").await.unwrap();
    db.migrate().await.unwrap();
    db
}

/// A migrated, uniquely named `MySQL` database, dropped by [`MySqlTestDb::drop`].
pub struct MySqlTestDb {
    pub db: Db,
    admin: MySqlPool,
    name: String,
}

impl MySqlTestDb {
    /// `None` when `SULTAN_TEST_MYSQL_URL` is not set.
    pub async fn create() -> Option<Self> {
        let test_db = Self::create_empty().await?;
        test_db.db.migrate().await.unwrap();
        Some(test_db)
    }

    /// Like [`create`](Self::create) but without running the migrations.
    pub async fn create_empty() -> Option<Self> {
        let url = std::env::var("SULTAN_TEST_MYSQL_URL").ok()?;
        let admin = MySqlPoolOptions::new()
            .max_connections(1)
            .connect(&url)
            .await
            .unwrap();
        let name = format!("sultan_t_{}", uuid::Uuid::now_v7().simple());
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
        Some(Self {
            db: Db::MySql(pool),
            admin,
            name,
        })
    }

    pub async fn drop(self) {
        self.db.close().await;
        sqlx::query(&format!("DROP DATABASE {}", self.name))
            .execute(&self.admin)
            .await
            .unwrap();
    }
}

/// Runs a raw statement (for checks the repository API deliberately does not offer).
pub async fn raw(db: &Db, sql: &str) -> Result<u64, sqlx::Error> {
    match db {
        Db::Sqlite(pool) => sqlx::query(sql)
            .execute(pool)
            .await
            .map(|r| r.rows_affected()),
        Db::MySql(pool) => sqlx::query(sql)
            .execute(pool)
            .await
            .map(|r| r.rows_affected()),
    }
}

/// Counts rows of a table.
pub async fn count(db: &Db, table: &str) -> i64 {
    let sql = format!("SELECT COUNT(*) FROM {table}");
    match db {
        Db::Sqlite(pool) => sqlx::query_scalar(&sql).fetch_one(pool).await.unwrap(),
        Db::MySql(pool) => sqlx::query_scalar(&sql).fetch_one(pool).await.unwrap(),
    }
}

pub const fn t(millis: i64) -> Timestamp {
    Timestamp::from_unix_millis(millis)
}

pub fn hash(n: u8) -> TokenHash {
    [n; 32]
}

/// A small organization: one company, two branches, a location and register at the first
/// branch, and two users.
pub struct Org {
    pub company: Company,
    pub jkt: Branch,
    pub bdg: Branch,
    pub location: StockLocation,
    pub register: Register,
    pub cashier: User,
    pub manager: User,
}

pub fn branch(company: CompanyId, code: &str) -> Branch {
    Branch {
        id: BranchId::generate(),
        company,
        code: code.to_owned(),
        name: format!("Toko {code}"),
        address: "Jl. Merdeka 1".to_owned(),
        npwp: None,
        receipt_header: "Terima kasih".to_owned(),
        receipt_footer: String::new(),
        timezone: "Asia/Jakarta".to_owned(),
        archived: false,
    }
}

pub fn user(username: &str) -> User {
    User {
        id: UserId::generate(),
        username: username.to_owned(),
        display_name: username.to_uppercase(),
        pin_hash: Some(format!("$argon2id$pin-{username}")),
        password_hash: None,
        active: true,
    }
}

pub async fn org(db: &Db) -> Org {
    let company = Company {
        id: CompanyId::generate(),
        name: "PT Sultan".to_owned(),
    };
    let jkt = branch(company.id, "JKT01");
    let bdg = branch(company.id, "BDG01");
    let location = StockLocation {
        id: LocationId::generate(),
        branch: jkt.id,
        name: "Store floor".to_owned(),
        receiving_default: true,
        archived: false,
    };
    let register = Register {
        id: RegisterId::generate(),
        branch: jkt.id,
        code: "R1".to_owned(),
        name: "Front".to_owned(),
        default_location: Some(location.id),
        archived: false,
    };
    let cashier = user("ani");
    let manager = user("budi");

    let now = t(1_000);
    let mut tx = db.begin().await.unwrap();
    sultan_db::org::save_company(&mut tx, &company, now)
        .await
        .unwrap();
    sultan_db::org::save_branch(&mut tx, &jkt, now)
        .await
        .unwrap();
    sultan_db::org::save_branch(&mut tx, &bdg, now)
        .await
        .unwrap();
    sultan_db::org::save_location(&mut tx, &location, now)
        .await
        .unwrap();
    sultan_db::org::save_register(&mut tx, &register, now)
        .await
        .unwrap();
    sultan_db::users::save_user(&mut tx, &cashier, now)
        .await
        .unwrap();
    sultan_db::users::save_user(&mut tx, &manager, now)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    Org {
        company,
        jkt,
        bdg,
        location,
        register,
        cashier,
        manager,
    }
}
