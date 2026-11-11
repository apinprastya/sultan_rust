//! The fake's back office (phase C4): password users with catalog permissions and an in-memory
//! catalog that follows the server's rules closely enough for the back-office UI tests:
//!
//! - `PUT /products/{id}` runs the core rules (`validate_product`, "frozen after use",
//!   bundle components) and answers `422` with `code`, `field` and `errors[]` like the server;
//!   a product code, SKU, barcode or PLU used by another product is `409 duplicate_*` with
//!   `product_id`, `variant_id`, `value` and `field`. Empty codes are generated (`P000001`),
//!   empty SKUs become `<code>-<value codes>`, a replay of the same body changes nothing.
//! - Writes need `catalog.manage` through any assignment (`403 permission_denied`; spec 2.2 §11,
//!   C3b); archived products need `catalog.view_archived`. Every write that changed something
//!   emits `catalog.changed`.
//! - `DELETE` of a product, category or brand (C3b, spec 2.2 §9): a used product (sold, see
//!   [`FakeService::mark_used`], or a component of another bundle), a category with products or
//!   children, a brand with products is `409 in_use` with `reason`, like the server.
//! - Categories (core tree rules), brands, tax classes, settings (scale layouts seeded like the
//!   server: 21 weight, 22 price), generators (with idempotency keys), branch blocks, images
//!   (PNG/JPEG/WebP magic bytes only, stored as sent) and search (paged by
//!   `after_variant_id`).
//! - The register's lookup ([`crate::Service::lookup`], phase C5) in `lookup`.
//! - Pricing (phase R3): price lists, entries, tax rates, settings, the quote and the lookup
//!   price in `pricing`.
//!
//! Users: `owner` / `owner-pass-1` (all-branches Owner), `budi` / `budi-pass-1` (Manager at one
//! branch: `catalog.manage` and `catalog.view_archived` there only, which is enough since C3b)
//! and `kasir` / `kasir-pass-1` (Cashier: no catalog permission, so the catalog is read-only).

mod lookup;
mod pricing;

pub use pricing::{PriceCustomer, PricingDemo};

use std::collections::{BTreeMap, HashMap};

use serde_json::{Value, json};
use sultan_core::catalog::{
    Brand as CoreBrand, CatalogError, Category as CoreCategory, ComponentInfo, ProductDraft,
    ProductType, Qty, ScaleLayout, check_frozen, default_sku, next_internal_barcode,
    next_product_code, validate_brand, validate_bundle_components, validate_category,
    validate_product, variant_display_name,
};
use sultan_core::ids::{BrandId, CategoryId, ProductId, UnitId, VariantId};

use super::{BRANCH_ID, FakeService, State, api, ready};
use crate::catalog::{
    Brand, BrandBody, CATALOG_MANAGE, CATALOG_VIEW_ARCHIVED, CatalogSettings, Category,
    CategoryBody, CategoryNode, Generate, ImageInfo, Me, Product, ProductPage, ProductQuery,
    ProductSummary, SearchHit, SearchPage, TaxClass, unit_kind,
};
use crate::{BackOffice, Branch, ClientError, Login, Problem, ServerEvent};

/// The seeded `Standard` tax class (the server's ID).
pub const STANDARD_TAX_CLASS: &str = "00000000-0000-7000-8000-000000000101";
/// The seeded `Exempt` tax class.
pub const EXEMPT_TAX_CLASS: &str = "00000000-0000-7000-8000-000000000102";
/// The fake's second branch.
pub const OTHER_BRANCH_ID: &str = "branch-2";

/// A back-office user of the fake.
#[allow(clippy::struct_excessive_bools)] // independent permission flags
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackOfficeUser {
    /// User ID.
    pub id: String,
    /// Login name.
    pub username: String,
    /// Password.
    pub password: String,
    /// Display name.
    pub name: String,
    /// `catalog.manage` for all branches.
    pub manage: bool,
    /// `catalog.manage` at the session branch only (enough for the catalog since C3b).
    pub manage_here: bool,
    /// `catalog.view_archived` (for all branches when `manage` is set, else at the session
    /// branch only; either is enough).
    pub view_archived: bool,
}

impl BackOfficeUser {
    /// A user without catalog permissions.
    #[must_use]
    pub fn new(id: &str, username: &str, password: &str, name: &str) -> Self {
        Self {
            id: id.to_owned(),
            username: username.to_owned(),
            password: password.to_owned(),
            name: name.to_owned(),
            manage: false,
            manage_here: false,
            view_archived: false,
        }
    }

    /// With `catalog.manage` and `catalog.view_archived` for all branches.
    #[must_use]
    pub const fn catalog_admin(mut self) -> Self {
        self.manage = true;
        self.view_archived = true;
        self
    }
}

#[derive(Debug, Clone)]
struct Stored {
    product: Product,
    used: bool,
}

/// The fake's catalog state.
pub(super) struct Catalog {
    users: Vec<BackOfficeUser>,
    tokens: HashMap<String, String>,
    products: BTreeMap<String, Stored>,
    categories: Vec<Category>,
    brands: Vec<Brand>,
    settings: CatalogSettings,
    code_counter: u64,
    barcode_counter: u64,
    generated: HashMap<String, String>,
    blocks: HashMap<String, Vec<String>>,
    images: HashMap<String, Vec<u8>>,
    events: Vec<ServerEvent>,
    pricing: pricing::Pricing,
}

impl Default for Catalog {
    fn default() -> Self {
        Self {
            users: vec![
                BackOfficeUser::new("u-owner", "owner", "owner-pass-1", "Owner").catalog_admin(),
                BackOfficeUser {
                    manage_here: true,
                    view_archived: true,
                    ..BackOfficeUser::new("u-budi", "budi", "budi-pass-1", "Budi")
                },
                BackOfficeUser::new("u-kasir", "kasir", "kasir-pass-1", "Kasir"),
            ],
            tokens: HashMap::new(),
            products: BTreeMap::new(),
            categories: Vec::new(),
            brands: Vec::new(),
            settings: CatalogSettings {
                product_code_prefix: "P".to_owned(),
                internal_barcode_prefix: "29".to_owned(),
                // The server's seed: 21 PPPPP WWWWW C (weight), 22 PPPPP $$$$$ C (price).
                scale_layouts: ScaleLayout::default_layouts()
                    .into_iter()
                    .map(|l| crate::catalog::ScaleLayout {
                        prefix: l.prefix,
                        plu_digits: l.plu_digits,
                        value_digits: l.value_digits,
                        kind: match l.kind {
                            sultan_core::catalog::ScaleValueKind::Weight => "weight",
                            sultan_core::catalog::ScaleValueKind::Price => "price",
                        }
                        .to_owned(),
                    })
                    .collect(),
                unit_names: ["pcs", "box", "pack", "renteng", "kg", "gram", "liter"]
                    .map(str::to_owned)
                    .to_vec(),
            },
            code_counter: 1,
            barcode_counter: 1,
            generated: HashMap::new(),
            blocks: HashMap::new(),
            images: HashMap::new(),
            events: Vec::new(),
            pricing: pricing::Pricing::default(),
        }
    }
}

impl Catalog {
    pub(super) fn has_token(&self, token: &str) -> bool {
        self.tokens.contains_key(token)
    }

    pub(super) fn remove_token(&mut self, token: &str) -> Option<String> {
        self.tokens.remove(token)
    }
}

fn denied(permission: &str) -> ClientError {
    Problem::new(403, "permission_denied", "denied")
        .with("permission", permission)
        .into()
}

fn not_found(what: &str) -> ClientError {
    Problem::new(404, "not_found", &format!("{what} not found")).into()
}

/// `409 in_use`, shaped like the server's (spec 2.2 §9).
fn in_use(reason: &str) -> ClientError {
    Problem::new(409, "in_use", "in use: archive it instead")
        .with("reason", reason)
        .into()
}

/// `422` for catalog rule failures, shaped like `ApiError::catalog_rules`.
fn rules(errors: &[CatalogError]) -> ClientError {
    let items: Vec<Value> = errors
        .iter()
        .map(|e| {
            let mut item = json!({ "code": e.code(), "message": e.to_string() });
            if let Some(field) = e.field() {
                item["field"] = field.into();
            }
            item
        })
        .collect();
    let first = &errors[0];
    let mut problem = Problem::new(422, first.code(), &first.to_string()).with("errors", items);
    if let Some(field) = first.field() {
        problem = problem.with("field", field);
    }
    problem.into()
}

/// `409 duplicate_*`, shaped like `ApiError::catalog_duplicate`.
fn duplicate(error: &CatalogError, product: &str, variant: Option<&str>) -> ClientError {
    let value: Value = match error {
        CatalogError::DuplicateCode { code } | CatalogError::DuplicateBarcode { code } => {
            code.as_str().into()
        }
        CatalogError::DuplicateSku { sku } => sku.as_str().into(),
        CatalogError::DuplicatePlu { plu } => (*plu).into(),
        _ => Value::Null,
    };
    Problem::new(409, error.code(), &error.to_string())
        .with("product_id", product)
        .with("variant_id", variant.map_or(Value::Null, Value::from))
        .with("value", value)
        .with("field", error.field().unwrap_or_default())
        .into()
}

fn changed(scope: &str, key: &str, id: &str) -> ServerEvent {
    ServerEvent {
        kind: "catalog.changed".to_owned(),
        data: json!({ "scope": scope, key: id }),
    }
}

/// Writes the normalised draft's values back into the document.
fn apply_draft(product: &mut Product, draft: &ProductDraft) {
    product.code.clone_from(&draft.code);
    product.name.clone_from(&draft.name);
    product.receipt_name.clone_from(&draft.receipt_name);
    product.tags.clone_from(&draft.tags);
    for (o, d) in product.options.iter_mut().zip(&draft.options) {
        o.name.clone_from(&d.name);
        for (v, dv) in o.values.iter_mut().zip(&d.values) {
            v.name.clone_from(&dv.name);
            v.code.clone_from(&dv.code);
        }
    }
    for (u, d) in product.units.iter_mut().zip(&draft.units.units) {
        u.name.clone_from(&d.name);
        u.short_name.clone_from(&d.short_name);
    }
    for (v, d) in product.variants.iter_mut().zip(&draft.variants) {
        v.sku.clone_from(&d.sku);
    }
    for (b, d) in product.barcodes.iter_mut().zip(&draft.barcodes) {
        b.code.clone_from(&d.code);
    }
    product.default_sale_unit_id = draft.units.default_sale.to_string();
    product.default_purchase_unit_id = draft.units.default_purchase.to_string();
}

/// What a stored document looks like without its answer-only members.
fn body_of(product: &Product) -> Product {
    let mut p = product.clone();
    p.id.clear();
    p.archived_barcodes.clear();
    p.archived_at = None;
    p.used = false;
    p.created_at = 0;
    p.updated_at = 0;
    for c in &mut p.components {
        c.product_id = None;
        c.name = None;
        c.sku = None;
        c.unit_name = None;
    }
    p
}

fn variant_name(product: &Product, variant: &crate::catalog::Variant) -> String {
    let names: Vec<&str> = variant
        .option_value_ids
        .iter()
        .filter_map(|id| {
            product
                .options
                .iter()
                .flat_map(|o| &o.values)
                .find(|v| v.id == *id)
                .map(|v| v.name.as_str())
        })
        .collect();
    variant_display_name(&product.name, &names)
}

fn magic_ok(bytes: &[u8]) -> bool {
    bytes.starts_with(b"\x89PNG")
        || bytes.starts_with(&[0xFF, 0xD8])
        || (bytes.len() > 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP")
}

impl State {
    pub(super) fn bo_user(&self, token: &str) -> Result<BackOfficeUser, ClientError> {
        self.catalog
            .tokens
            .get(token)
            .and_then(|id| self.catalog.users.iter().find(|u| u.id == *id))
            .cloned()
            .ok_or_else(|| api(401, "invalid_session"))
    }

    fn manager(&self, token: &str) -> Result<BackOfficeUser, ClientError> {
        let user = self.bo_user(token)?;
        if user.manage || user.manage_here {
            Ok(user)
        } else {
            Err(denied(CATALOG_MANAGE))
        }
    }

    fn may_view_archived(&self, token: &str) -> Result<(), ClientError> {
        if self.bo_user(token)?.view_archived {
            Ok(())
        } else {
            Err(denied(CATALOG_VIEW_ARCHIVED))
        }
    }

    /// The document with its component lines labelled, like the server's answer.
    fn answer(&self, stored: &Stored) -> Product {
        let mut p = stored.product.clone();
        p.used = stored.used;
        for c in &mut p.components {
            let found = self.catalog.products.values().find_map(|s| {
                s.product
                    .variants
                    .iter()
                    .find(|v| v.id == c.variant_id)
                    .map(|v| (s, v))
            });
            if let Some((s, v)) = found {
                c.product_id = Some(s.product.id.clone());
                c.name = Some(variant_name(&s.product, v));
                c.sku = Some(v.sku.clone());
                c.unit_name = s
                    .product
                    .units
                    .iter()
                    .find(|u| u.id == c.unit_id)
                    .map(|u| u.name.clone());
            }
        }
        p
    }

    fn code_taken(&self, code: &str) -> bool {
        self.catalog
            .products
            .values()
            .any(|s| s.product.code == code)
    }

    fn next_code(&mut self) -> Result<String, ClientError> {
        let prefix = self.catalog.settings.product_code_prefix.clone();
        let generated =
            next_product_code(&prefix, self.catalog.code_counter, |c| self.code_taken(c))
                .map_err(|e| rules(&[e]))?;
        self.catalog.code_counter = generated.next_counter;
        Ok(generated.value)
    }

    fn next_barcode(&mut self) -> Result<String, ClientError> {
        let prefix = self.catalog.settings.internal_barcode_prefix.clone();
        let taken = |code: &str| {
            self.catalog
                .products
                .values()
                .any(|s| s.product.barcodes.iter().any(|b| b.code == code))
        };
        let generated = next_internal_barcode(
            &prefix,
            self.catalog.barcode_counter,
            &ScaleLayout::default_layouts(),
            taken,
        )
        .map_err(|e| rules(&[e]))?;
        self.catalog.barcode_counter = generated.next_counter;
        Ok(generated.value)
    }

    /// The first code, SKU, barcode or PLU of `product` used by an active other product.
    fn find_duplicate(&self, id: &str, product: &Product) -> Option<ClientError> {
        for other in self.catalog.products.values() {
            let o = &other.product;
            if o.id == id {
                continue;
            }
            if o.code == product.code {
                let e = CatalogError::DuplicateCode {
                    code: product.code.clone(),
                };
                return Some(duplicate(&e, &o.id, None));
            }
            if o.archived {
                continue;
            }
            for v in product.variants.iter().filter(|v| !v.archived) {
                if let Some(ov) = o.variants.iter().find(|ov| !ov.archived && ov.sku == v.sku) {
                    let e = CatalogError::DuplicateSku { sku: v.sku.clone() };
                    return Some(duplicate(&e, &o.id, Some(&ov.id)));
                }
                if let Some(ov) = v.plu.and_then(|plu| {
                    o.variants
                        .iter()
                        .find(|ov| !ov.archived && ov.plu == Some(plu))
                }) {
                    let e = CatalogError::DuplicatePlu {
                        plu: v.plu.unwrap_or_default(),
                    };
                    return Some(duplicate(&e, &o.id, Some(&ov.id)));
                }
            }
            for b in &product.barcodes {
                if let Some(ob) = o.barcodes.iter().find(|ob| ob.code == b.code) {
                    let e = CatalogError::DuplicateBarcode {
                        code: b.code.clone(),
                    };
                    return Some(duplicate(&e, &o.id, Some(&ob.variant_id)));
                }
            }
        }
        None
    }

    fn component_info(&self, variant: VariantId, unit: UnitId) -> Option<ComponentInfo> {
        let (v, u) = (variant.to_string(), unit.to_string());
        self.catalog.products.values().find_map(|s| {
            let p = &s.product;
            let pv = p.variants.iter().find(|x| x.id == v)?;
            let pu = p.units.iter().find(|x| x.id == u)?;
            Some(ComponentInfo {
                product: p.id.parse().ok()?,
                product_type: p.product_type.parse().ok()?,
                archived: p.archived || pv.archived || pu.archived,
                unit_kind: unit_kind(&pu.kind).ok()?,
                factor: Qty::from_thousandths(pu.factor_thousandths),
            })
        })
    }

    #[allow(clippy::too_many_lines)] // the server's write path, step by step
    fn put_product(
        &mut self,
        token: &str,
        id: &str,
        body: &Product,
    ) -> Result<Product, ClientError> {
        self.manager(token)?;
        let product_id: ProductId = id
            .parse()
            .map_err(|_| ClientError::from(Problem::new(400, "invalid_body", "bad id")))?;
        let stored = self.catalog.products.get(id).cloned();
        let mut draft = body.to_draft(id).map_err(ClientError::from)?;
        if draft.code.trim().is_empty() {
            draft.code = match &stored {
                Some(s) => s.product.code.clone(),
                None => self.next_code()?,
            };
        }
        let mut draft = draft.normalized();
        for variant in &mut draft.variants {
            if variant.sku.is_empty() {
                let codes: Vec<&str> = variant
                    .values
                    .iter()
                    .filter_map(|v| {
                        draft
                            .options
                            .iter()
                            .find_map(|o| o.value(*v))
                            .map(|v| v.code.as_str())
                    })
                    .collect();
                variant.sku = default_sku(&draft.code, &codes);
            }
        }
        validate_product(&draft).map_err(|e| rules(&e))?;
        if let Some(s) = stored.as_ref().filter(|s| s.used) {
            let old = s.product.to_draft(id).map_err(ClientError::from)?;
            check_frozen(&old, &draft, true).map_err(|e| rules(&[e]))?;
        }
        let tax_known =
            [STANDARD_TAX_CLASS, EXEMPT_TAX_CLASS].contains(&body.tax_class_id.as_str());
        let category_known = body
            .category_id
            .as_ref()
            .is_none_or(|c| self.catalog.categories.iter().any(|x| x.id == *c));
        let brand_known = body
            .brand_id
            .as_ref()
            .is_none_or(|b| self.catalog.brands.iter().any(|x| x.id == *b));
        if !(tax_known && category_known && brand_known) {
            return Err(api(422, "invalid_reference"));
        }
        if draft.product_type == ProductType::Bundle {
            let own = draft.clone();
            validate_bundle_components(product_id, &draft.components, |v, u| {
                if own.variants.iter().any(|x| x.id == v) {
                    return Some(ComponentInfo {
                        product: own.id,
                        product_type: own.product_type,
                        archived: false,
                        unit_kind: sultan_core::catalog::UnitKind::Integer,
                        factor: Qty::ONE,
                    });
                }
                self.component_info(v, u)
            })
            .map_err(|e| rules(&[e]))?;
        }
        let mut product = body.clone();
        apply_draft(&mut product, &draft);
        id.clone_into(&mut product.id);
        if let Some(e) = self.find_duplicate(id, &product) {
            return Err(e);
        }
        let now = self.now;
        if let Some(s) = &stored {
            if body_of(&s.product) == body_of(&product) {
                return Ok(self.answer(s));
            }
            product.created_at = s.product.created_at;
            product.archived_at = if product.archived {
                s.product.archived_at.or(Some(now))
            } else {
                None
            };
        } else {
            product.created_at = now;
            product.archived_at = product.archived.then_some(now);
        }
        product.updated_at = now;
        let entry = Stored {
            product,
            used: stored.as_ref().is_some_and(|s| s.used),
        };
        self.catalog.products.insert(id.to_owned(), entry.clone());
        self.catalog
            .events
            .push(changed("product", "product_id", id));
        Ok(self.answer(&entry))
    }

    fn set_archived(
        &mut self,
        token: &str,
        id: &str,
        archived: bool,
    ) -> Result<Product, ClientError> {
        self.manager(token)?;
        let mut entry = self
            .catalog
            .products
            .get(id)
            .cloned()
            .ok_or_else(|| not_found("product"))?;
        if entry.product.archived == archived {
            return Ok(self.answer(&entry));
        }
        if !archived && let Some(e) = self.find_duplicate(id, &entry.product) {
            return Err(e);
        }
        entry.product.archived = archived;
        entry.product.archived_at = archived.then_some(self.now);
        entry.product.updated_at = self.now;
        self.catalog.products.insert(id.to_owned(), entry.clone());
        self.catalog
            .events
            .push(changed("product", "product_id", id));
        Ok(self.answer(&entry))
    }

    fn core_categories(&self) -> Vec<CoreCategory> {
        self.catalog
            .categories
            .iter()
            .filter_map(|c| {
                Some(CoreCategory {
                    id: c.id.parse().ok()?,
                    parent: c.parent_id.as_ref().and_then(|p| p.parse().ok()),
                    name: c.name.clone(),
                    code: c.code.clone(),
                    archived: c.archived,
                })
            })
            .collect()
    }

    fn put_category(
        &mut self,
        token: &str,
        id: &str,
        body: &CategoryBody,
    ) -> Result<Category, ClientError> {
        self.manager(token)?;
        let bad = || ClientError::from(Problem::new(400, "invalid_body", "bad id"));
        let core_id: CategoryId = id.parse().map_err(|_| bad())?;
        let parent: Option<CategoryId> = body
            .parent_id
            .as_ref()
            .map(|p| p.parse().map_err(|_| bad()))
            .transpose()?;
        let category = CoreCategory {
            id: core_id,
            parent,
            name: body.name.trim().to_owned(),
            code: body.code.trim().to_ascii_uppercase(),
            archived: body.archived,
        };
        validate_category(&self.core_categories(), &category).map_err(|e| rules(&[e]))?;
        let stored = Category {
            id: id.to_owned(),
            parent_id: body.parent_id.clone(),
            name: category.name,
            code: category.code,
            archived: body.archived,
        };
        match self.catalog.categories.iter_mut().find(|c| c.id == id) {
            Some(existing) if *existing == stored => return Ok(stored),
            Some(existing) => *existing = stored.clone(),
            None => self.catalog.categories.push(stored.clone()),
        }
        self.catalog
            .events
            .push(changed("category", "category_id", id));
        Ok(stored)
    }

    fn put_brand(&mut self, token: &str, id: &str, body: &BrandBody) -> Result<Brand, ClientError> {
        self.manager(token)?;
        let core_id: BrandId = id
            .parse()
            .map_err(|_| ClientError::from(Problem::new(400, "invalid_body", "bad id")))?;
        let existing: Vec<CoreBrand> = self
            .catalog
            .brands
            .iter()
            .filter_map(|b| {
                Some(CoreBrand {
                    id: b.id.parse().ok()?,
                    name: b.name.clone(),
                    archived: b.archived,
                })
            })
            .collect();
        let brand = CoreBrand {
            id: core_id,
            name: body.name.trim().to_owned(),
            archived: body.archived,
        };
        validate_brand(&existing, &brand).map_err(|e| rules(&[e]))?;
        let stored = Brand {
            id: id.to_owned(),
            name: brand.name,
            archived: body.archived,
        };
        match self.catalog.brands.iter_mut().find(|b| b.id == id) {
            Some(existing) if *existing == stored => return Ok(stored),
            Some(existing) => *existing = stored.clone(),
            None => self.catalog.brands.push(stored.clone()),
        }
        self.catalog.brands.sort_by(|a, b| a.name.cmp(&b.name));
        self.catalog.events.push(changed("brand", "brand_id", id));
        Ok(stored)
    }

    fn tree(&self, parent: Option<&str>, include_archived: bool) -> Vec<CategoryNode> {
        let mut nodes: Vec<CategoryNode> = self
            .catalog
            .categories
            .iter()
            .filter(|c| c.parent_id.as_deref() == parent && (include_archived || !c.archived))
            .map(|c| CategoryNode {
                id: c.id.clone(),
                parent_id: c.parent_id.clone(),
                name: c.name.clone(),
                code: c.code.clone(),
                archived: c.archived,
                children: self.tree(Some(&c.id), include_archived),
            })
            .collect();
        nodes.sort_by(|a, b| a.name.cmp(&b.name));
        nodes
    }

    fn list(&self, token: &str, query: &ProductQuery) -> Result<ProductPage, ClientError> {
        self.bo_user(token)?;
        if query.archived {
            self.may_view_archived(token)?;
        }
        let limit = query.limit.unwrap_or(50) as usize;
        let q = query
            .q
            .as_deref()
            .map(str::to_lowercase)
            .unwrap_or_default();
        let brand_name = |id: &Option<String>| {
            id.as_ref()
                .and_then(|b| self.catalog.brands.iter().find(|x| x.id == *b))
                .map(|b| b.name.to_lowercase())
                .unwrap_or_default()
        };
        let items: Vec<ProductSummary> = self
            .catalog
            .products
            .values()
            .rev()
            .map(|s| &s.product)
            .filter(|p| query.before_id.as_ref().is_none_or(|b| p.id < *b))
            .filter(|p| p.archived == query.archived)
            .filter(|p| {
                q.is_empty()
                    || p.name.to_lowercase().contains(&q)
                    || p.code.to_lowercase().contains(&q)
                    || brand_name(&p.brand_id).contains(&q)
            })
            .filter(|p| query.category_id.is_none() || p.category_id == query.category_id)
            .filter(|p| query.brand_id.is_none() || p.brand_id == query.brand_id)
            .filter(|p| {
                query
                    .product_type
                    .as_ref()
                    .is_none_or(|t| p.product_type == *t)
            })
            .take(limit)
            .map(|p| ProductSummary {
                id: p.id.clone(),
                code: p.code.clone(),
                name: p.name.clone(),
                product_type: p.product_type.clone(),
                category_id: p.category_id.clone(),
                brand_id: p.brand_id.clone(),
                sellable: p.sellable,
                archived_at: p.archived_at,
                variant_count: u32::try_from(p.variants.iter().filter(|v| !v.archived).count())
                    .unwrap_or(u32::MAX),
            })
            .collect();
        let next_before_id = items
            .last()
            .filter(|_| items.len() == limit)
            .map(|p| p.id.clone());
        Ok(ProductPage {
            items,
            next_before_id,
        })
    }

    fn search(
        &self,
        token: &str,
        q: &str,
        limit: u32,
        after: Option<&str>,
    ) -> Result<SearchPage, ClientError> {
        self.bo_user(token)?;
        let q = q.trim().to_lowercase();
        if q.is_empty() {
            return Ok(SearchPage::default());
        }
        let mut hits = Vec::new();
        for s in self.catalog.products.values() {
            let p = &s.product;
            if p.archived || !p.sellable {
                continue;
            }
            for v in p.variants.iter().filter(|v| !v.archived) {
                let name = variant_name(p, v);
                if name.to_lowercase().contains(&q) || v.sku.to_lowercase().contains(&q) {
                    hits.push(SearchHit {
                        product_id: p.id.clone(),
                        variant_id: v.id.clone(),
                        unit_id: p.default_sale_unit_id.clone(),
                        name,
                        sku: v.sku.clone(),
                        product_type: p.product_type.clone(),
                    });
                }
            }
        }
        if let Some(after) = after {
            // An unknown cursor ends the search, like the server.
            let start = hits
                .iter()
                .position(|h| h.variant_id == after)
                .map_or(hits.len(), |i| i + 1);
            hits.drain(..start);
        }
        hits.truncate(limit as usize);
        let next_after_variant_id = hits
            .last()
            .filter(|_| hits.len() == limit as usize)
            .map(|h| h.variant_id.clone());
        Ok(SearchPage {
            items: hits,
            next_after_variant_id,
        })
    }

    fn delete_product(&mut self, token: &str, id: &str) -> Result<(), ClientError> {
        self.manager(token)?;
        let stored = self
            .catalog
            .products
            .get(id)
            .ok_or_else(|| not_found("product"))?;
        if stored.used {
            return Err(in_use("used"));
        }
        let component = self.catalog.products.values().any(|other| {
            other.product.id != id
                && other
                    .product
                    .components
                    .iter()
                    .any(|c| stored.product.variants.iter().any(|v| v.id == c.variant_id))
        });
        if component {
            return Err(in_use("bundle_component"));
        }
        self.catalog.products.remove(id);
        self.catalog.images.remove(id);
        self.catalog.blocks.remove(id);
        self.catalog
            .events
            .push(changed("product", "product_id", id));
        Ok(())
    }

    fn delete_category(&mut self, token: &str, id: &str) -> Result<(), ClientError> {
        self.manager(token)?;
        if !self.catalog.categories.iter().any(|c| c.id == id) {
            return Err(not_found("category"));
        }
        let products = self.catalog.products.values();
        if products
            .into_iter()
            .any(|s| s.product.category_id.as_deref() == Some(id))
        {
            return Err(in_use("has_products"));
        }
        if self
            .catalog
            .categories
            .iter()
            .any(|c| c.parent_id.as_deref() == Some(id))
        {
            return Err(in_use("has_children"));
        }
        self.catalog.categories.retain(|c| c.id != id);
        self.catalog
            .events
            .push(changed("category", "category_id", id));
        Ok(())
    }

    fn delete_brand(&mut self, token: &str, id: &str) -> Result<(), ClientError> {
        self.manager(token)?;
        if !self.catalog.brands.iter().any(|b| b.id == id) {
            return Err(not_found("brand"));
        }
        if self
            .catalog
            .products
            .values()
            .any(|s| s.product.brand_id.as_deref() == Some(id))
        {
            return Err(in_use("has_products"));
        }
        self.catalog.brands.retain(|b| b.id != id);
        self.catalog.events.push(changed("brand", "brand_id", id));
        Ok(())
    }
}

impl FakeService {
    /// Adds a back-office user.
    pub fn add_backoffice_user(&self, user: BackOfficeUser) {
        self.state().catalog.users.push(user);
    }

    /// Marks a stored product as used (a sale referenced it): "frozen after use" applies.
    pub fn mark_used(&self, product_id: &str) {
        if let Some(s) = self.state().catalog.products.get_mut(product_id) {
            s.used = true;
        }
    }

    /// The stored product (as `GET` answers it).
    #[must_use]
    pub fn stored_product(&self, id: &str) -> Option<Product> {
        let s = self.state();
        s.catalog.products.get(id).map(|p| s.answer(p))
    }

    /// Every stored product ID, oldest first.
    #[must_use]
    pub fn product_ids(&self) -> Vec<String> {
        self.state().catalog.products.keys().cloned().collect()
    }

    /// The stored image bytes of a product.
    #[must_use]
    pub fn stored_image(&self, id: &str) -> Option<Vec<u8>> {
        self.state().catalog.images.get(id).cloned()
    }

    /// The blocked branches of a product.
    #[must_use]
    pub fn stored_blocks(&self, id: &str) -> Vec<String> {
        self.state()
            .catalog
            .blocks
            .get(id)
            .cloned()
            .unwrap_or_default()
    }

    /// The stored categories (flat).
    #[must_use]
    pub fn stored_categories(&self) -> Vec<Category> {
        self.state().catalog.categories.clone()
    }

    /// The stored brands.
    #[must_use]
    pub fn stored_brands(&self) -> Vec<Brand> {
        self.state().catalog.brands.clone()
    }

    /// Runs a write as the owner without recording a call (test and screenshot setup).
    ///
    /// # Panics
    /// If the write is rejected.
    pub fn seed_product(&self, id: &str, product: &Product) -> Product {
        let mut s = self.state();
        s.catalog.tokens.insert("seed".into(), "u-owner".into());
        let result = s.put_product("seed", id, product);
        s.catalog.tokens.remove("seed");
        s.catalog.events.clear();
        result.unwrap_or_else(|e| panic!("seed product: {e}"))
    }

    /// Adds a category as the owner (setup).
    ///
    /// # Panics
    /// If the write is rejected.
    pub fn seed_category(&self, id: &str, body: &CategoryBody) {
        let mut s = self.state();
        s.catalog.tokens.insert("seed".into(), "u-owner".into());
        let result = s.put_category("seed", id, body);
        s.catalog.tokens.remove("seed");
        s.catalog.events.clear();
        result.unwrap_or_else(|e| panic!("seed category: {e}"));
    }

    /// Adds a brand as the owner (setup).
    ///
    /// # Panics
    /// If the write is rejected.
    pub fn seed_brand(&self, id: &str, name: &str) {
        let mut s = self.state();
        s.catalog.tokens.insert("seed".into(), "u-owner".into());
        let body = BrandBody {
            name: name.to_owned(),
            archived: false,
        };
        let result = s.put_brand("seed", id, &body);
        s.catalog.tokens.remove("seed");
        s.catalog.events.clear();
        result.unwrap_or_else(|e| panic!("seed brand: {e}"));
    }
}

/// Finishes a catalog call: drops the lock, then sends the events the call produced.
fn finish<T: Send + 'static>(
    mut s: std::sync::MutexGuard<'_, State>,
    result: Result<T, ClientError>,
) -> crate::BoxFuture<T> {
    let events = std::mem::take(&mut s.catalog.events);
    let sink = s.sink.clone();
    drop(s);
    if let Some(sink) = sink {
        for event in events {
            sink(event);
        }
    }
    ready(result)
}

macro_rules! call {
    ($self:ident, $method:literal, $detail:expr, |$s:ident| $body:expr) => {{
        // Reads do not need the guard mutably; writes do.
        #[allow(unused_mut)]
        let (mut $s, failure) = $self.begin($method, $detail);
        let result = match failure {
            Some(e) => Err(e),
            None => $body,
        };
        finish($s, result)
    }};
}

impl BackOffice for FakeService {
    fn price_lists(
        &self,
        token: &str,
        include_archived: bool,
    ) -> crate::BoxFuture<Vec<crate::PriceList>> {
        call!(self, "price_lists", include_archived.to_string(), |s| s
            .price_lists(token, include_archived))
    }

    fn put_price_list(
        &self,
        token: &str,
        id: &str,
        body: &crate::PriceListBody,
    ) -> crate::BoxFuture<crate::PriceList> {
        call!(self, "put_price_list", id.to_owned(), |s| s
            .put_price_list(token, id, body))
    }

    fn archive_price_list(
        &self,
        token: &str,
        id: &str,
        key: &str,
    ) -> crate::BoxFuture<crate::PriceList> {
        call!(self, "archive_price_list", format!("{id} key={key}"), |s| s
            .archive_price_list(token, id))
    }

    fn price_entries(
        &self,
        token: &str,
        list_id: &str,
        variant_id: Option<&str>,
        after: Option<&str>,
    ) -> crate::BoxFuture<crate::EntryPage> {
        call!(self, "price_entries", list_id.to_owned(), |s| s
            .price_entries(token, list_id, variant_id, after))
    }

    fn put_price_entry(
        &self,
        token: &str,
        list_id: &str,
        id: &str,
        body: &crate::PriceEntryBody,
    ) -> crate::BoxFuture<crate::PriceEntry> {
        call!(self, "put_price_entry", format!("{list_id}/{id}"), |s| s
            .put_price_entry(token, list_id, id, body))
    }

    fn delete_price_entry(&self, token: &str, list_id: &str, id: &str) -> crate::BoxFuture<()> {
        call!(self, "delete_price_entry", format!("{list_id}/{id}"), |s| s
            .delete_price_entry(token, list_id, id))
    }

    fn product_prices(
        &self,
        token: &str,
        product_id: &str,
    ) -> crate::BoxFuture<crate::ProductPrices> {
        call!(self, "product_prices", product_id.to_owned(), |s| s
            .product_prices(token, product_id))
    }

    fn tax_rates(&self, token: &str, tax_class_id: &str) -> crate::BoxFuture<Vec<crate::TaxRate>> {
        call!(self, "tax_rates", tax_class_id.to_owned(), |s| s
            .tax_rates(token, tax_class_id))
    }

    fn put_tax_rate(
        &self,
        token: &str,
        tax_class_id: &str,
        rate: &crate::TaxRate,
    ) -> crate::BoxFuture<Vec<crate::TaxRate>> {
        call!(self, "put_tax_rate", tax_class_id.to_owned(), |s| s
            .put_tax_rate(token, tax_class_id, rate))
    }

    fn pricing_settings(&self, token: &str) -> crate::BoxFuture<crate::PricingSettings> {
        call!(self, "pricing_settings", String::new(), |s| s
            .pricing_settings(token))
    }

    fn put_pricing_settings(
        &self,
        token: &str,
        body: &crate::PricingSettings,
    ) -> crate::BoxFuture<crate::PricingSettings> {
        call!(self, "put_pricing_settings", String::new(), |s| s
            .put_pricing_settings(token, body))
    }

    fn password_login(&self, username: &str, password: &str) -> crate::BoxFuture<Login> {
        call!(self, "password_login", username.to_owned(), |s| {
            let user = s
                .catalog
                .users
                .iter()
                .find(|u| u.username == username && u.password == password)
                .cloned();
            match user {
                None => Err(api(401, "invalid_credentials")),
                Some(user) => {
                    let token = s.next("bo-token");
                    s.catalog.tokens.insert(token.clone(), user.id.clone());
                    Ok(Login {
                        token,
                        user_id: user.id,
                        display_name: user.name,
                        branch_id: BRANCH_ID.to_owned(),
                        register_id: None,
                        expires_at: s.now + 30 * 60 * 1_000,
                    })
                }
            }
        })
    }

    fn me(&self, token: &str) -> crate::BoxFuture<Me> {
        call!(self, "me", String::new(), |s| s.bo_user(token).map(|u| {
            let mut everywhere = Vec::new();
            let mut here = Vec::new();
            if u.manage {
                everywhere.push(CATALOG_MANAGE.to_owned());
            }
            if u.view_archived && u.manage {
                everywhere.push(CATALOG_VIEW_ARCHIVED.to_owned());
            }
            if u.manage || u.manage_here {
                here.push(CATALOG_MANAGE.to_owned());
            }
            if u.view_archived {
                here.push(CATALOG_VIEW_ARCHIVED.to_owned());
            }
            // Customer permissions follow the seeded roles (see `customers`).
            let role = if u.manage {
                super::customers::Role::Owner
            } else if u.manage_here {
                super::customers::Role::Manager
            } else {
                super::customers::Role::Cashier
            };
            here.extend(role.keys());
            if u.manage {
                everywhere.extend(role.keys());
            }
            // `price.manage` goes with `catalog.manage` in the fake (pricing module docs).
            if u.manage {
                everywhere.push(crate::pricing::PRICE_MANAGE.to_owned());
            }
            if u.manage || u.manage_here {
                here.push(crate::pricing::PRICE_MANAGE.to_owned());
            }
            let anywhere = here.clone();
            Me {
                user_id: u.id,
                username: u.username,
                display_name: u.name,
                branch_id: BRANCH_ID.to_owned(),
                register_id: None,
                permissions: here,
                permissions_everywhere: everywhere,
                permissions_anywhere: anywhere,
            }
        }))
    }

    fn branches(&self, token: &str) -> crate::BoxFuture<Vec<Branch>> {
        call!(self, "branches", String::new(), |s| s.bo_user(token).map(
            |_| {
                vec![
                    Branch {
                        id: BRANCH_ID.to_owned(),
                        code: "JKT01".to_owned(),
                        name: "Toko Jakarta".to_owned(),
                        timezone: "Asia/Jakarta".to_owned(),
                    },
                    Branch {
                        id: OTHER_BRANCH_ID.to_owned(),
                        code: "BDG01".to_owned(),
                        name: "Toko Bandung".to_owned(),
                        timezone: "Asia/Jakarta".to_owned(),
                    },
                ]
            }
        ))
    }

    fn products(&self, token: &str, query: &ProductQuery) -> crate::BoxFuture<ProductPage> {
        let detail = serde_json::to_string(query).unwrap_or_default();
        call!(self, "products", detail, |s| s.list(token, query))
    }

    fn product(&self, token: &str, id: &str) -> crate::BoxFuture<Product> {
        call!(self, "product", id.to_owned(), |s| {
            s.bo_user(token).and_then(|_| {
                let stored = s
                    .catalog
                    .products
                    .get(id)
                    .cloned()
                    .ok_or_else(|| not_found("product"))?;
                if stored.product.archived {
                    s.may_view_archived(token)?;
                }
                Ok(s.answer(&stored))
            })
        })
    }

    fn put_product(&self, token: &str, id: &str, body: &Product) -> crate::BoxFuture<Product> {
        call!(self, "put_product", id.to_owned(), |s| s
            .put_product(token, id, body))
    }

    fn set_archived(
        &self,
        token: &str,
        id: &str,
        archived: bool,
        key: &str,
    ) -> crate::BoxFuture<Product> {
        let detail = format!("{id} archived={archived} key={key}");
        call!(self, "set_archived", detail, |s| s
            .set_archived(token, id, archived))
    }

    fn categories(
        &self,
        token: &str,
        include_archived: bool,
    ) -> crate::BoxFuture<Vec<CategoryNode>> {
        call!(self, "categories", include_archived.to_string(), |s| {
            s.bo_user(token).and_then(|_| {
                if include_archived {
                    s.may_view_archived(token)?;
                }
                Ok(s.tree(None, include_archived))
            })
        })
    }

    fn put_category(
        &self,
        token: &str,
        id: &str,
        body: &CategoryBody,
    ) -> crate::BoxFuture<Category> {
        call!(self, "put_category", id.to_owned(), |s| s
            .put_category(token, id, body))
    }

    fn brands(&self, token: &str, include_archived: bool) -> crate::BoxFuture<Vec<Brand>> {
        call!(self, "brands", include_archived.to_string(), |s| {
            s.bo_user(token).and_then(|_| {
                if include_archived {
                    s.may_view_archived(token)?;
                }
                Ok(s.catalog
                    .brands
                    .iter()
                    .filter(|b| include_archived || !b.archived)
                    .cloned()
                    .collect())
            })
        })
    }

    fn put_brand(&self, token: &str, id: &str, body: &BrandBody) -> crate::BoxFuture<Brand> {
        call!(self, "put_brand", id.to_owned(), |s| s
            .put_brand(token, id, body))
    }

    fn tax_classes(&self, token: &str) -> crate::BoxFuture<Vec<TaxClass>> {
        call!(self, "tax_classes", String::new(), |s| s
            .bo_user(token)
            .map(|_| {
                vec![
                    TaxClass {
                        id: EXEMPT_TAX_CLASS.to_owned(),
                        name: "Exempt".to_owned(),
                        archived: false,
                    },
                    TaxClass {
                        id: STANDARD_TAX_CLASS.to_owned(),
                        name: "Standard".to_owned(),
                        archived: false,
                    },
                ]
            }))
    }

    fn catalog_settings(&self, token: &str) -> crate::BoxFuture<CatalogSettings> {
        call!(self, "catalog_settings", String::new(), |s| s
            .bo_user(token)
            .map(|_| s.catalog.settings.clone()))
    }

    fn generate(&self, token: &str, what: Generate, key: &str) -> crate::BoxFuture<String> {
        let detail = format!("{} key={key}", what.path());
        call!(self, "generate", detail, |s| {
            s.manager(token).and_then(|_| {
                let replay = format!("{}:{key}", what.path());
                if let Some(value) = s.catalog.generated.get(&replay) {
                    return Ok(value.clone());
                }
                let value = match what {
                    Generate::ProductCode => s.next_code()?,
                    Generate::Barcode => s.next_barcode()?,
                };
                s.catalog.generated.insert(replay, value.clone());
                Ok(value)
            })
        })
    }

    fn branch_blocks(&self, token: &str, id: &str) -> crate::BoxFuture<Vec<String>> {
        call!(self, "branch_blocks", id.to_owned(), |s| {
            s.bo_user(token).and_then(|_| {
                if !s.catalog.products.contains_key(id) {
                    return Err(not_found("product"));
                }
                Ok(s.catalog.blocks.get(id).cloned().unwrap_or_default())
            })
        })
    }

    fn put_branch_blocks(
        &self,
        token: &str,
        id: &str,
        branch_ids: &[String],
    ) -> crate::BoxFuture<Vec<String>> {
        call!(
            self,
            "put_branch_blocks",
            format!("{id} {branch_ids:?}"),
            |s| {
                s.manager(token).and_then(|_| {
                    if !s.catalog.products.contains_key(id) {
                        return Err(not_found("product"));
                    }
                    if branch_ids
                        .iter()
                        .any(|b| b != BRANCH_ID && b != OTHER_BRANCH_ID)
                    {
                        return Err(Problem::new(422, "invalid_field", "unknown branch")
                            .with("field", "branch_ids")
                            .into());
                    }
                    let mut set = branch_ids.to_vec();
                    set.sort();
                    set.dedup();
                    if s.catalog.blocks.get(id).cloned().unwrap_or_default() != set {
                        s.catalog.blocks.insert(id.to_owned(), set.clone());
                        s.catalog.events.push(changed("product", "product_id", id));
                    }
                    Ok(set)
                })
            }
        )
    }

    fn product_image(&self, token: &str, id: &str) -> crate::BoxFuture<Option<Vec<u8>>> {
        call!(self, "product_image", id.to_owned(), |s| s
            .bo_user(token)
            .map(|_| s.catalog.images.get(id).cloned()))
    }

    fn put_product_image(
        &self,
        token: &str,
        id: &str,
        bytes: Vec<u8>,
    ) -> crate::BoxFuture<ImageInfo> {
        call!(
            self,
            "put_product_image",
            format!("{id} {} bytes", bytes.len()),
            |s| {
                s.manager(token).and_then(|_| {
                    if !s.catalog.products.contains_key(id) {
                        return Err(not_found("product"));
                    }
                    if !magic_ok(&bytes) {
                        return Err(api(422, "invalid_image"));
                    }
                    let info = ImageInfo {
                        product_id: id.to_owned(),
                        mime: "image/jpeg".to_owned(),
                        width: 0,
                        height: 0,
                        bytes: bytes.len(),
                        etag: format!("\"{}\"", bytes.len()),
                    };
                    s.catalog.images.insert(id.to_owned(), bytes);
                    s.catalog.events.push(changed("product", "product_id", id));
                    Ok(info)
                })
            }
        )
    }

    fn delete_product_image(&self, token: &str, id: &str) -> crate::BoxFuture<()> {
        call!(self, "delete_product_image", id.to_owned(), |s| {
            s.manager(token).and_then(|_| {
                if !s.catalog.products.contains_key(id) {
                    return Err(not_found("product"));
                }
                if s.catalog.images.remove(id).is_some() {
                    s.catalog.events.push(changed("product", "product_id", id));
                }
                Ok(())
            })
        })
    }

    fn search(
        &self,
        token: &str,
        q: &str,
        limit: u32,
        after: Option<&str>,
    ) -> crate::BoxFuture<SearchPage> {
        call!(self, "search", q.to_owned(), |s| s
            .search(token, q, limit, after))
    }

    fn delete_product(&self, token: &str, id: &str) -> crate::BoxFuture<()> {
        call!(self, "delete_product", id.to_owned(), |s| s
            .delete_product(token, id))
    }

    fn delete_category(&self, token: &str, id: &str) -> crate::BoxFuture<()> {
        call!(self, "delete_category", id.to_owned(), |s| s
            .delete_category(token, id))
    }

    fn delete_brand(&self, token: &str, id: &str) -> crate::BoxFuture<()> {
        call!(self, "delete_brand", id.to_owned(), |s| s
            .delete_brand(token, id))
    }

    fn customer(&self, token: &str, id: &str) -> crate::BoxFuture<crate::Customer> {
        self.bo_customer(token, id)
    }

    fn put_customer(
        &self,
        token: &str,
        id: &str,
        body: &crate::CustomerBody,
    ) -> crate::BoxFuture<crate::Customer> {
        self.put_customer_call("put_customer", token, id, body)
    }

    fn set_customer_archived(
        &self,
        token: &str,
        id: &str,
        archived: bool,
        key: &str,
    ) -> crate::BoxFuture<crate::Customer> {
        self.bo_set_customer_archived(token, id, archived, key)
    }

    fn merge_customer(
        &self,
        token: &str,
        into: &str,
        from: &str,
        key: &str,
    ) -> crate::BoxFuture<crate::Customer> {
        self.bo_merge_customer(token, into, from, key)
    }

    fn erase_customer(
        &self,
        token: &str,
        id: &str,
        key: &str,
    ) -> crate::BoxFuture<crate::Customer> {
        self.bo_erase_customer(token, id, key)
    }

    fn customer_duplicates(&self, token: &str, page: u32) -> crate::BoxFuture<crate::CustomerPage> {
        self.bo_review(token, page, false)
    }

    fn negative_balances(&self, token: &str, page: u32) -> crate::BoxFuture<crate::CustomerPage> {
        self.bo_review(token, page, true)
    }

    fn customer_groups(
        &self,
        token: &str,
        include_archived: bool,
    ) -> crate::BoxFuture<Vec<crate::CustomerGroup>> {
        self.bo_customer_groups(token, include_archived)
    }

    fn put_customer_group(
        &self,
        token: &str,
        id: &str,
        body: &crate::GroupBody,
    ) -> crate::BoxFuture<crate::CustomerGroup> {
        self.bo_put_customer_group(token, id, body)
    }

    fn membership_tiers(
        &self,
        token: &str,
        include_archived: bool,
    ) -> crate::BoxFuture<Vec<crate::MembershipTier>> {
        self.bo_membership_tiers(token, include_archived)
    }

    fn put_membership_tier(
        &self,
        token: &str,
        id: &str,
        body: &crate::TierBody,
    ) -> crate::BoxFuture<crate::MembershipTier> {
        self.bo_put_membership_tier(token, id, body)
    }

    fn customer_balance_settings(&self, token: &str) -> crate::BoxFuture<crate::BalanceSettings> {
        self.bo_balance_settings(token)
    }

    fn put_customer_balance_settings(
        &self,
        token: &str,
        body: &crate::BalanceSettings,
    ) -> crate::BoxFuture<crate::BalanceSettings> {
        self.bo_put_balance_settings(token, body)
    }
}
