//! Shared SQL plumbing: bind values, row access and the three statement runners. Every query in
//! the crate goes through [`execute`], [`fetch_optional`] or [`fetch_all`], so the per-backend
//! dispatch lives only here.

use sqlx::Row as _;
use sqlx::mysql::MySqlRow;
use sqlx::sqlite::SqliteRow;
use sultan_core::ids::{
    BarcodeId, BranchId, BrandId, CashierSessionId, CategoryId, CompanyId, LocationId, OptionId,
    OptionValueId, ProductId, RegisterId, RoleId, TaxClassId, UnitId, UserId, VariantId,
};
use sultan_core::money::Money;
use sultan_core::time::Timestamp;
use uuid::Uuid;

use crate::conn::{Conn, Tx, with_connection};
use crate::error::{DbError, Result};
use crate::{Saved, TokenHash};

/// One bind value. The variants are the column types the schema uses.
#[derive(Debug, Clone)]
pub(crate) enum Arg {
    Bytes(Vec<u8>),
    OptBytes(Option<Vec<u8>>),
    I64(i64),
    OptI64(Option<i64>),
    Text(String),
    OptText(Option<String>),
    Bool(bool),
}

/// `args![a, b, c]` = `vec![Arg::from(a), Arg::from(b), Arg::from(c)]`.
macro_rules! args {
    ($($value:expr),* $(,)?) => {
        vec![$($crate::sql::Arg::from($value)),*]
    };
}
pub(crate) use args;

macro_rules! arg_from_id {
    ($($id:ty),*) => {$(
        impl From<$id> for Arg {
            fn from(id: $id) -> Self {
                Self::Bytes(id.as_uuid().as_bytes().to_vec())
            }
        }
        impl From<Option<$id>> for Arg {
            fn from(id: Option<$id>) -> Self {
                Self::OptBytes(id.map(|id| id.as_uuid().as_bytes().to_vec()))
            }
        }
    )*};
}
arg_from_id!(
    BarcodeId,
    BranchId,
    BrandId,
    CashierSessionId,
    CategoryId,
    CompanyId,
    LocationId,
    OptionId,
    OptionValueId,
    ProductId,
    RegisterId,
    RoleId,
    TaxClassId,
    UnitId,
    UserId,
    VariantId
);

impl From<Uuid> for Arg {
    fn from(id: Uuid) -> Self {
        Self::Bytes(id.as_bytes().to_vec())
    }
}
impl From<Vec<u8>> for Arg {
    fn from(bytes: Vec<u8>) -> Self {
        Self::Bytes(bytes)
    }
}
impl From<&TokenHash> for Arg {
    fn from(hash: &TokenHash) -> Self {
        Self::Bytes(hash.to_vec())
    }
}
impl From<i64> for Arg {
    fn from(v: i64) -> Self {
        Self::I64(v)
    }
}
impl From<Option<i64>> for Arg {
    fn from(v: Option<i64>) -> Self {
        Self::OptI64(v)
    }
}
impl From<Timestamp> for Arg {
    fn from(t: Timestamp) -> Self {
        Self::I64(t.unix_millis())
    }
}
impl From<Option<Timestamp>> for Arg {
    fn from(t: Option<Timestamp>) -> Self {
        Self::OptI64(t.map(Timestamp::unix_millis))
    }
}
impl From<Money> for Arg {
    fn from(m: Money) -> Self {
        Self::I64(m.minor())
    }
}
impl From<&str> for Arg {
    fn from(s: &str) -> Self {
        Self::Text(s.to_owned())
    }
}
impl From<String> for Arg {
    fn from(s: String) -> Self {
        Self::Text(s)
    }
}
impl From<Option<&str>> for Arg {
    fn from(s: Option<&str>) -> Self {
        Self::OptText(s.map(str::to_owned))
    }
}
impl From<bool> for Arg {
    fn from(b: bool) -> Self {
        Self::Bool(b)
    }
}

/// Builds a `sqlx::query` with `$args` bound in order. Expanded inside [`with_connection`], so
/// it is compiled once per backend.
macro_rules! bound_query {
    ($sql:expr, $args:expr) => {{
        let mut query = sqlx::query($sql);
        for arg in $args {
            query = match arg {
                Arg::Bytes(v) => query.bind(v),
                Arg::OptBytes(v) => query.bind(v),
                Arg::I64(v) => query.bind(v),
                Arg::OptI64(v) => query.bind(v),
                Arg::Text(v) => query.bind(v),
                Arg::OptText(v) => query.bind(v),
                Arg::Bool(v) => query.bind(v),
            };
        }
        query
    }};
}

/// Runs a statement; returns the number of affected rows.
pub(crate) async fn execute(conn: Conn<'_>, sql: &str, args: Vec<Arg>) -> Result<u64> {
    with_connection!(conn, |c| {
        let done = bound_query!(sql, args).execute(&mut *c).await?;
        Ok(done.rows_affected())
    })
}

/// Runs a query and maps the first row, if any.
pub(crate) async fn fetch_optional<T>(
    conn: Conn<'_>,
    sql: &str,
    args: Vec<Arg>,
    map: impl Fn(&dyn Row) -> Result<T>,
) -> Result<Option<T>> {
    with_connection!(conn, |c| {
        let row = bound_query!(sql, args).fetch_optional(&mut *c).await?;
        row.as_ref().map(|r| map(r)).transpose()
    })
}

/// Runs a query and maps every row.
pub(crate) async fn fetch_all<T>(
    conn: Conn<'_>,
    sql: &str,
    args: Vec<Arg>,
    map: impl Fn(&dyn Row) -> Result<T>,
) -> Result<Vec<T>> {
    with_connection!(conn, |c| {
        let rows = bound_query!(sql, args).fetch_all(&mut *c).await?;
        rows.iter().map(|r| map(r)).collect()
    })
}

/// Inserts or overwrites the row `id` of `table` (an idempotent `PUT`, spec 2.1 §8).
///
/// `columns` and `values` exclude `id`, `created_at` and `updated_at`, which are handled here.
/// The existence check and the write run in the caller's transaction. A select-then-write is used
/// instead of `ON CONFLICT` / `ON DUPLICATE KEY` because the latter in `MySQL` fires on *any*
/// unique key and would silently overwrite another row (e.g. a duplicate username).
pub(crate) async fn upsert_by_id(
    tx: &mut Tx,
    table: &'static str,
    id: Arg,
    columns: &[&'static str],
    mut values: Vec<Arg>,
    now: Timestamp,
) -> Result<Saved> {
    let exists = fetch_optional(
        Conn::Tx(&mut *tx),
        &format!("SELECT 1 FROM {table} WHERE id = ?"),
        vec![id.clone()],
        |_| Ok(()),
    )
    .await?
    .is_some();
    values.push(now.into());
    values.push(id);
    let sql = if exists {
        let sets: Vec<String> = columns.iter().map(|c| format!("{c} = ?")).collect();
        format!(
            "UPDATE {table} SET {}, updated_at = ? WHERE id = ?",
            sets.join(", ")
        )
    } else {
        values.push(now.into());
        let placeholders = vec!["?"; columns.len() + 3].join(", ");
        format!(
            "INSERT INTO {table} ({}, updated_at, id, created_at) VALUES ({placeholders})",
            columns.join(", ")
        )
    };
    execute(Conn::Tx(&mut *tx), &sql, values).await?;
    Ok(if exists {
        Saved::Updated
    } else {
        Saved::Created
    })
}

/// Typed column access, implemented for the row types of both backends.
pub(crate) trait Row {
    fn bytes(&self, col: &str) -> Result<Vec<u8>>;
    fn opt_bytes(&self, col: &str) -> Result<Option<Vec<u8>>>;
    fn i64(&self, col: &str) -> Result<i64>;
    fn opt_i64(&self, col: &str) -> Result<Option<i64>>;
    fn text(&self, col: &str) -> Result<String>;
    fn opt_text(&self, col: &str) -> Result<Option<String>>;
    fn bool(&self, col: &str) -> Result<bool>;
    /// A JSON column: `TEXT` on `SQLite`, `JSON` on `MySQL`. Both deliver UTF-8 text, so the
    /// value is read without sqlx's column-type check.
    fn opt_json_text(&self, col: &str) -> Result<Option<String>>;
}

/// `$text` is how text columns are read: checked on `SQLite`; unchecked on `MySQL`, because the
/// schema's byte-wise `utf8mb4_0900_bin` collation (Phase 9) makes the server flag character
/// columns `BINARY`, which sqlx 0.8 refuses to decode as `String`. The value is still decoded as
/// UTF-8 (sqlx's `str` decoding validates it); only the column-type check is skipped.
macro_rules! impl_row {
    ($row:ty, $text:ident) => {
        impl Row for $row {
            fn bytes(&self, col: &str) -> Result<Vec<u8>> {
                Ok(self.try_get(col)?)
            }
            fn opt_bytes(&self, col: &str) -> Result<Option<Vec<u8>>> {
                Ok(self.try_get(col)?)
            }
            fn i64(&self, col: &str) -> Result<i64> {
                Ok(self.try_get(col)?)
            }
            fn opt_i64(&self, col: &str) -> Result<Option<i64>> {
                Ok(self.try_get(col)?)
            }
            fn text(&self, col: &str) -> Result<String> {
                Ok(self.$text(col)?)
            }
            fn opt_text(&self, col: &str) -> Result<Option<String>> {
                Ok(self.$text(col)?)
            }
            fn bool(&self, col: &str) -> Result<bool> {
                Ok(self.try_get(col)?)
            }
            fn opt_json_text(&self, col: &str) -> Result<Option<String>> {
                Ok(self.try_get_unchecked(col)?)
            }
        }
    };
}
impl_row!(SqliteRow, try_get);
impl_row!(MySqlRow, try_get_unchecked);

/// Reads a 16-byte UUID column as an ID type.
pub(crate) fn id<T: From<Uuid>>(row: &dyn Row, col: &str) -> Result<T> {
    uuid_from(&row.bytes(col)?, col).map(T::from)
}

/// Reads a nullable 16-byte UUID column as an ID type.
pub(crate) fn opt_id<T: From<Uuid>>(row: &dyn Row, col: &str) -> Result<Option<T>> {
    row.opt_bytes(col)?
        .map(|b| uuid_from(&b, col).map(T::from))
        .transpose()
}

/// Reads a timestamp column.
pub(crate) fn ts(row: &dyn Row, col: &str) -> Result<Timestamp> {
    row.i64(col).map(Timestamp::from_unix_millis)
}

/// Reads a nullable timestamp column.
pub(crate) fn opt_ts(row: &dyn Row, col: &str) -> Result<Option<Timestamp>> {
    Ok(row.opt_i64(col)?.map(Timestamp::from_unix_millis))
}

/// Reads a money column.
pub(crate) fn money(row: &dyn Row, col: &str) -> Result<Money> {
    row.i64(col).map(Money::from_minor)
}

/// Reads a 32-byte hash column.
pub(crate) fn hash(row: &dyn Row, col: &str) -> Result<TokenHash> {
    TokenHash::try_from(row.bytes(col)?.as_slice())
        .map_err(|_| DbError::Corrupt(format!("column `{col}` is not a 32-byte hash")))
}

/// Parses a JSON column.
pub(crate) fn opt_json(row: &dyn Row, col: &str) -> Result<Option<serde_json::Value>> {
    row.opt_json_text(col)?
        .map(|text| {
            serde_json::from_str(&text)
                .map_err(|e| DbError::Corrupt(format!("column `{col}` is not JSON: {e}")))
        })
        .transpose()
}

fn uuid_from(bytes: &[u8], col: &str) -> Result<Uuid> {
    Uuid::from_slice(bytes)
        .map_err(|_| DbError::Corrupt(format!("column `{col}` is not a 16-byte UUID")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bad_uuid_bytes_are_corrupt() {
        let err = uuid_from(&[1, 2, 3], "id").unwrap_err();
        assert_eq!(
            err.to_string(),
            "invalid stored data: column `id` is not a 16-byte UUID"
        );
    }
}
