//! The catalog repositories (spec `docs/spec/2.2-catalog.md`, phase C2): products with their
//! options, units, variants, barcodes and bundle components; categories, brands and tax classes;
//! branch assortment; product images; catalog settings and code generators; the register lookup
//! and search.
//!
//! Every rule lives in `sultan_core::catalog`; this module loads the inputs, runs the `core`
//! checks and stores the result. Writes take a [`Tx`](crate::Tx) and append **one audit row and
//! one outbox row in the same transaction** (`docs/plan.md` §1, spec 2.2 §11). A write that
//! changes nothing (an idempotent replay) appends neither and answers [`Written::Unchanged`].
//!
//! Company-wide uniqueness (product code, active SKUs, active barcodes, PLUs of active weighed
//! variants) is checked inside the transaction so the error can name the conflicting variant
//! ([`Duplicate`]); unique indexes on `*_active` columns are the safety net against races, which
//! then surface as a retryable [`DbError::Conflict`].

mod assortment;
mod json;
mod lookup;
mod product;
mod settings;
mod taxonomy;

use std::fmt;

use serde_json::Value;
use sultan_core::catalog::CatalogError;
use sultan_core::ids::{BranchId, ProductId, RegisterId, UserId, VariantId};
use sultan_core::time::Timestamp;

use crate::audit::{self, AuditEntry};
use crate::conn::Tx;
use crate::error::DbError;
use crate::outbox;

pub use assortment::{
    ProductImage, delete_product_image, get_branch_blocks, get_product_image, put_branch_blocks,
    put_product_image,
};
pub use lookup::{
    DbCatalogIndex, LookupItem, MAX_SEARCH_LIMIT, SearchHit, VariantLabel, load_index, lookup,
    search, search_page, variant_labels,
};
pub use product::{
    MAX_LIST_LIMIT, ProductDoc, ProductFilter, ProductRecord, ProductSummary, archive_product,
    delete_product, get_product, list_products, mark_used, put_product, unarchive_product,
};
pub use settings::{
    CatalogSettings, generate_internal_barcode, generate_product_code, get_catalog_settings,
    put_catalog_settings,
};
pub use taxonomy::{
    CategoryNode, EXEMPT_TAX_CLASS, STANDARD_TAX_CLASS, TaxClass, delete_brand, delete_category,
    get_brand, get_category, list_brands, list_categories, list_tax_classes, put_brand,
    put_category,
};

/// Who makes a catalog change, for the audit row (spec 2.1 §7).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct Actor {
    /// The acting user, if any (`None` for system jobs such as an import).
    pub user: Option<UserId>,
    /// The branch the change was made at.
    pub branch: Option<BranchId>,
    /// The register the change was made at.
    pub register: Option<RegisterId>,
}

impl Actor {
    /// An actor that is only a user (back office at HQ).
    #[must_use]
    pub const fn user(user: UserId) -> Self {
        Self {
            user: Some(user),
            branch: None,
            register: None,
        }
    }
}

/// What a catalog write did. Unlike [`Saved`](crate::Saved) it also reports an idempotent replay
/// that changed nothing (no audit or outbox row was written).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Written {
    /// A new row (or document) was created.
    Created,
    /// An existing row (or document) changed.
    Updated,
    /// The stored state already matched: nothing was written.
    Unchanged,
}

/// A company-wide uniqueness conflict: the product code, SKU, barcode or PLU is used by another
/// product (spec 2.2 §12: `409` with the conflicting variant).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Duplicate {
    /// [`CatalogError::DuplicateCode`], [`CatalogError::DuplicateSku`],
    /// [`CatalogError::DuplicateBarcode`] or [`CatalogError::DuplicatePlu`].
    pub error: CatalogError,
    /// The product that already uses the value.
    pub product: ProductId,
    /// Its variant (`None` for a product code).
    pub variant: Option<VariantId>,
}

impl fmt::Display for Duplicate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} (product {}", self.error, self.product)?;
        if let Some(variant) = self.variant {
            write!(f, ", variant {variant}")?;
        }
        f.write_str(")")
    }
}

/// Errors of the catalog repositories.
#[derive(Debug, thiserror::Error)]
pub enum CatalogDbError {
    /// The change breaks catalog rules (validation, "frozen after use", bundle rules, unknown
    /// references) or a lookup failed (`not_found`, `archived`, `not_sold_here`, …). Never empty.
    #[error("{}", join(.0))]
    Rule(Vec<CatalogError>),
    /// A company-wide uniqueness conflict.
    #[error("{0}")]
    Duplicate(Duplicate),
    /// The entity the operation needs does not exist.
    #[error("not found: {0}")]
    NotFound(&'static str),
    /// A hard delete of a row that is referenced (spec 2.2 §9: archive it instead). The reason
    /// is `used` (a variant was sold, moved or priced), `bundle_component` (a variant is in
    /// another bundle), `has_products` or `has_children`.
    #[error("in use: {0}")]
    InUse(&'static str),
    /// A database error. A lost race is [`DbError::Conflict`]: see
    /// [`CatalogDbError::is_retryable`].
    #[error(transparent)]
    Db(#[from] DbError),
}

fn join(errors: &[CatalogError]) -> String {
    errors
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("; ")
}

impl CatalogDbError {
    /// A single rule error.
    #[must_use]
    pub fn rule(error: CatalogError) -> Self {
        Self::Rule(vec![error])
    }

    /// The stable problem+json code (spec 2.2 §12) of a rule or duplicate error (the first one
    /// for several rule errors); `not_found` for [`CatalogDbError::NotFound`]; `None` for a
    /// database error.
    #[must_use]
    pub fn code(&self) -> Option<&'static str> {
        match self {
            Self::Rule(errors) => errors.first().map(CatalogError::code),
            Self::Duplicate(d) => Some(d.error.code()),
            Self::NotFound(_) => Some("not_found"),
            Self::InUse(_) => Some("in_use"),
            Self::Db(_) => None,
        }
    }

    /// Whether running the whole transaction again may succeed (a lost race on a unique key or
    /// a counter, a deadlock, a busy `SQLite` database): see [`DbError::is_retryable`].
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        matches!(self, Self::Db(e) if e.is_retryable())
    }
}

/// Result alias of the catalog repositories.
pub type CatalogResult<T> = std::result::Result<T, CatalogDbError>;

/// One catalog change as recorded: an audit row and an outbox row.
struct Change<'a> {
    action: &'a str,
    target: String,
    before: Option<Value>,
    after: Option<Value>,
    outbox_kind: &'a str,
    payload: Value,
}

/// Appends the audit row and the outbox row of `change` in `tx`.
async fn record(
    tx: &mut Tx,
    actor: &Actor,
    now: Timestamp,
    change: Change<'_>,
) -> CatalogResult<()> {
    let mut entry = AuditEntry::new(now, change.action);
    entry.user = actor.user;
    entry.branch = actor.branch;
    entry.register = actor.register;
    entry.target = Some(change.target);
    entry.before = change.before;
    entry.after = change.after;
    audit::append(&mut *tx, &entry).await?;
    outbox::append(&mut *tx, change.outbox_kind, &change.payload, now).await?;
    Ok(())
}

/// How many times the generators run their transaction after a lost race.
const MAX_ATTEMPTS: usize = 8;

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    #[test]
    fn error_codes_and_messages() {
        let product = ProductId::from_uuid(Uuid::from_u128(1));
        let variant = VariantId::from_uuid(Uuid::from_u128(2));
        let dup = Duplicate {
            error: CatalogError::DuplicateSku { sku: "A".into() },
            product,
            variant: Some(variant),
        };
        assert_eq!(
            dup.to_string(),
            "SKU `A` is already used (product 00000000-0000-0000-0000-000000000001, \
             variant 00000000-0000-0000-0000-000000000002)"
        );
        let code = Duplicate {
            error: CatalogError::DuplicateCode { code: "P1".into() },
            product,
            variant: None,
        };
        assert_eq!(
            code.to_string(),
            "product code `P1` is already used (product 00000000-0000-0000-0000-000000000001)"
        );
        let cases: [(CatalogDbError, Option<&str>, bool); 6] = [
            (CatalogDbError::InUse("used"), Some("in_use"), false),
            (
                CatalogDbError::Rule(vec![CatalogError::NotFound, CatalogError::Archived]),
                Some("not_found"),
                false,
            ),
            (CatalogDbError::Duplicate(dup), Some("duplicate_sku"), false),
            (
                CatalogDbError::NotFound("product"),
                Some("not_found"),
                false,
            ),
            (
                CatalogDbError::Db(DbError::Conflict("x".into())),
                None,
                true,
            ),
            (CatalogDbError::Db(DbError::NotFound("x")), None, false),
        ];
        for (error, code, retryable) in cases {
            assert_eq!(error.code(), code, "{error}");
            assert_eq!(error.is_retryable(), retryable, "{error}");
        }
        assert_eq!(
            CatalogDbError::Rule(vec![CatalogError::NotFound, CatalogError::Archived]).to_string(),
            "not found; item is archived"
        );
        assert_eq!(
            CatalogDbError::rule(CatalogError::NotFound).code(),
            Some("not_found")
        );
        assert_eq!(
            CatalogDbError::NotFound("brand").to_string(),
            "not found: brand"
        );
        assert_eq!(
            CatalogDbError::InUse("has_products").to_string(),
            "in use: has_products"
        );
        let user = UserId::from_uuid(Uuid::from_u128(3));
        assert_eq!(Actor::user(user).user, Some(user));
        assert_eq!(Actor::default().user, None);
    }
}
