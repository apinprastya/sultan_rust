//! The product form of the back office (phase C4, spec 2.2 §2–§6): the editable state of one
//! product document, as typed, with no Slint and no IO.
//!
//! - **Text is kept as typed.** Codes are upper-cased, factors parsed and SKUs defaulted only
//!   when the document is built ([`ProductForm::to_body`]), so the view never rewrites what the
//!   user is typing. A variant SKU left empty follows the product code: the view shows the
//!   default (`CODE-M-RED`) as a placeholder and the body carries it.
//! - **Variant grid.** [`ProductForm::regenerate_grid`] builds the cartesian product of the
//!   option values. Each combination has an on/off switch: off means "not created" for a new
//!   combination and "archived" for a stored variant. New combinations from a new value start
//!   on; combinations a loaded product never had start off.
//! - **Units.** Row 0 is the base unit (factor 1); up to [`MAX_EXTRA_UNITS`] more.
//! - **Rules before save.** [`ProductForm::validate`] checks the typed numbers, then runs
//!   `sultan_core::catalog::validate_product` on the document, and turns every failure into a
//!   [`FormError`] on a tab, a field and, when it can tell, a row. [`ProductForm::apply_problem`]
//!   does the same for the server's `422 errors[]` and `409 duplicate_*` answers.
//! - **Frozen after use.** [`ProductForm::used`] locks the type, the base unit and its kind,
//!   stored unit factors and the option set; stored values and units can only be archived.
//! - **Dirty check.** [`ProductForm::is_dirty`] compares the editable state with the one loaded
//!   (or last saved).

use std::collections::BTreeSet;

use sultan_client::catalog::{
    Barcode, Component, OptionValue, Product, ProductOption, Unit, Variant, unit_kind,
    unit_kind_str,
};
use sultan_client::{FieldError, Problem};
use sultan_core::catalog::{
    CatalogError, CodeKind, MAX_EXTRA_UNITS, MAX_OPTIONS, ProductType, Qty, UnitKind,
    default_receipt_name, default_sku, validate_product,
};

/// The form's tabs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum Tab {
    /// Code, names, type, category, brand, tax, flags, image, branches.
    #[default]
    General,
    /// Options and the variant grid.
    Variants,
    /// Units, barcodes and bundle components.
    Units,
}

impl Tab {
    /// Every tab, in order.
    pub const ALL: [Self; 3] = [Self::General, Self::Variants, Self::Units];

    /// Index for the view.
    #[must_use]
    pub const fn index(self) -> usize {
        match self {
            Self::General => 0,
            Self::Variants => 1,
            Self::Units => 2,
        }
    }

    /// The tab of a body field (`variants.sku`, `units.factor_thousandths`, …).
    #[must_use]
    pub fn of_field(field: &str) -> Self {
        let head = field.split('.').next().unwrap_or_default();
        match head {
            "options" | "variants" => Self::Variants,
            "units"
            | "base_unit"
            | "base_unit_id"
            | "default_sale_unit_id"
            | "default_purchase_unit_id"
            | "barcodes"
            | "components" => Self::Units,
            _ => Self::General,
        }
    }
}

/// One error shown on the form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormError {
    /// The tab it is on.
    pub tab: Tab,
    /// The body field, e.g. `name`, `variants.sku`, `units.factor_thousandths`.
    pub field: String,
    /// The row (combination, unit, barcode, component or option), when known.
    pub row: Option<usize>,
    /// Message code for the view (catalog error codes and a few of the form's own).
    pub code: String,
    /// The `{}` argument (a value, a unit name…).
    pub arg: String,
}

impl FormError {
    fn new(field: &str, row: Option<usize>, code: &str, arg: impl Into<String>) -> Self {
        Self {
            tab: Tab::of_field(field),
            field: field.to_owned(),
            row,
            code: code.to_owned(),
            arg: arg.into(),
        }
    }

    /// Whether it belongs to `field` (exactly, or as its area: `variants` covers
    /// `variants.sku`) and, when `row` is given, that row.
    #[must_use]
    pub fn is_for(&self, field: &str, row: Option<usize>) -> bool {
        (self.field == field || self.field.starts_with(&format!("{field}.")))
            && (row.is_none() || self.row == row)
    }
}

/// A value of an option.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValueRow {
    /// ID.
    pub id: String,
    /// Display name.
    pub name: String,
    /// Short code as typed.
    pub code: String,
    /// Stored on the server (cannot be removed once the product is used).
    pub stored: bool,
}

/// An option with its values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OptionRow {
    /// ID.
    pub id: String,
    /// Name.
    pub name: String,
    /// Values.
    pub values: Vec<ValueRow>,
    /// Stored on the server.
    pub stored: bool,
}

/// One combination of the variant grid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Combo {
    /// One value ID per option (empty without options).
    pub value_ids: Vec<String>,
    /// The variant ID (kept when switched off and on again).
    pub variant_id: String,
    /// Sold: created (new) or active (stored).
    pub on: bool,
    /// SKU typed by the user; empty = the default.
    pub sku: String,
    /// PLU as typed (weighed).
    pub plu: String,
    /// Stored on the server: off means archived.
    pub stored: bool,
}

/// A unit row; row 0 is the base unit.
#[allow(clippy::struct_excessive_bools)] // independent unit flags from spec 2.2 §4
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnitRow {
    /// ID.
    pub id: String,
    /// Name.
    pub name: String,
    /// Receipt short name.
    pub short_name: String,
    /// Factor to the base unit as typed (`24`, `0.5`).
    pub factor: String,
    /// Sellable in this unit.
    pub sellable: bool,
    /// Purchasable in this unit.
    pub purchasable: bool,
    /// Archived (replaced packaging).
    pub archived: bool,
    /// Stored on the server (factor frozen after use).
    pub stored: bool,
}

/// A barcode row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BarcodeRow {
    /// ID.
    pub id: String,
    /// Variant.
    pub variant_id: String,
    /// Unit.
    pub unit_id: String,
    /// Code as typed.
    pub code: String,
}

/// A bundle component row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComponentRow {
    /// Variant of another product.
    pub variant_id: String,
    /// Its unit.
    pub unit_id: String,
    /// Quantity as typed.
    pub qty: String,
    /// The component's product, once known.
    pub product_id: Option<String>,
    /// Display name.
    pub name: String,
    /// SKU.
    pub sku: String,
    /// The product's active units `(id, name)`, once loaded.
    pub units: Vec<(String, String)>,
}

/// A pending change of the product image.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum ImageEdit {
    /// No change.
    #[default]
    Keep,
    /// Upload these bytes on save.
    Replace(Vec<u8>),
    /// Remove the image on save.
    Remove,
}

/// Everything the user can change; compared for the dirty check.
#[allow(clippy::struct_excessive_bools)] // independent product flags from spec 2.2 §2
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormData {
    /// Product code as typed (empty = generated on save).
    pub code: String,
    /// Name.
    pub name: String,
    /// Receipt name (empty = the name cut to 32).
    pub receipt_name: String,
    /// Type.
    pub product_type: ProductType,
    /// Category.
    pub category_id: Option<String>,
    /// Brand.
    pub brand_id: Option<String>,
    /// Tax class.
    pub tax_class_id: String,
    /// Tags, comma-separated as typed.
    pub tags: String,
    /// Open price.
    pub open_price: bool,
    /// Requires a line note.
    pub requires_note: bool,
    /// Sellable.
    pub sellable: bool,
    /// Purchasable.
    pub purchasable: bool,
    /// Options.
    pub options: Vec<OptionRow>,
    /// The variant grid.
    pub combos: Vec<Combo>,
    /// The base unit's kind (all units share it).
    pub base_kind: UnitKind,
    /// Units; row 0 is the base unit.
    pub units: Vec<UnitRow>,
    /// Default sale unit ID.
    pub default_sale: String,
    /// Default purchase unit ID.
    pub default_purchase: String,
    /// Barcodes.
    pub barcodes: Vec<BarcodeRow>,
    /// Bundle components.
    pub components: Vec<ComponentRow>,
    /// Branches where the product is not sold.
    pub blocked: BTreeSet<String>,
    /// Image change.
    pub image: ImageEdit,
}

/// A fresh ID for a new product or child row (`UUIDv7`, spec 2.2 §12).
#[must_use]
pub fn new_id() -> String {
    uuid::Uuid::now_v7().to_string()
}

/// The parsed product type (stock for anything unknown).
fn product_type(value: &str) -> ProductType {
    value.parse().unwrap_or(ProductType::Stock)
}

/// A factor or quantity in thousandths as text: `24000` → `24`, `500` → `0.5`.
#[must_use]
pub fn qty_text(thousandths: i64) -> String {
    let sign = if thousandths < 0 { "-" } else { "" };
    let abs = thousandths.unsigned_abs();
    let (whole, frac) = (abs / 1000, abs % 1000);
    if frac == 0 {
        format!("{sign}{whole}")
    } else {
        let frac = format!("{frac:03}");
        format!("{sign}{whole}.{}", frac.trim_end_matches('0'))
    }
}

/// The first `` `…` `` part of a message (the value the server names).
fn backticked(message: &str) -> Option<&str> {
    let start = message.find('`')? + 1;
    let len = message[start..].find('`')?;
    Some(&message[start..start + len])
}

/// The view's message code for a product-type rule (core reasons are English text).
fn type_rule_code(reason: &str) -> &'static str {
    if reason.contains("decimal") {
        "rule_weighed_decimal"
    } else if reason.contains("besides the base unit") {
        "rule_single_unit"
    } else if reason.contains("no options") {
        "rule_bundle_options"
    } else if reason.contains("must be integer") {
        "rule_bundle_integer"
    } else if reason.contains("only a bundle") {
        "rule_components"
    } else {
        "product_type_rule"
    }
}

/// The product form; see the module docs.
#[allow(clippy::struct_excessive_bools)] // independent state flags
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProductForm {
    /// Product ID (client-generated for a new product).
    pub id: String,
    /// Not stored yet.
    pub is_new: bool,
    /// A variant has been used: the "frozen after use" fields are read-only.
    pub used: bool,
    /// Archived.
    pub archived: bool,
    /// `updated_at` of the loaded version (to spot changes made elsewhere).
    pub updated_at: i64,
    /// The current tab.
    pub tab: Tab,
    /// Errors from the last check or save.
    pub errors: Vec<FormError>,
    /// The stored product has an image.
    pub has_image: bool,
    /// The editable state.
    pub data: FormData,
    baseline: FormData,
}

impl ProductForm {
    /// A new stock product: unit `pcs`, one variant, the given tax class.
    #[must_use]
    pub fn new_product(tax_class_id: &str) -> Self {
        let pcs = new_id();
        let data = FormData {
            code: String::new(),
            name: String::new(),
            receipt_name: String::new(),
            product_type: ProductType::Stock,
            category_id: None,
            brand_id: None,
            tax_class_id: tax_class_id.to_owned(),
            tags: String::new(),
            open_price: false,
            requires_note: false,
            sellable: true,
            purchasable: true,
            options: Vec::new(),
            combos: vec![Combo {
                value_ids: Vec::new(),
                variant_id: new_id(),
                on: true,
                sku: String::new(),
                plu: String::new(),
                stored: false,
            }],
            base_kind: UnitKind::Integer,
            units: vec![UnitRow {
                id: pcs.clone(),
                name: "pcs".to_owned(),
                short_name: String::new(),
                factor: "1".to_owned(),
                sellable: true,
                purchasable: true,
                archived: false,
                stored: false,
            }],
            default_sale: pcs.clone(),
            default_purchase: pcs,
            barcodes: Vec::new(),
            components: Vec::new(),
            blocked: BTreeSet::new(),
            image: ImageEdit::Keep,
        };
        Self {
            id: new_id(),
            is_new: true,
            used: false,
            archived: false,
            updated_at: 0,
            tab: Tab::General,
            errors: Vec::new(),
            has_image: false,
            baseline: data.clone(),
            data,
        }
    }

    /// The form of a stored product, with its blocked branches.
    #[must_use]
    #[allow(clippy::too_many_lines)] // one block per child list
    pub fn from_product(p: &Product, blocked: &[String], has_image: bool) -> Self {
        let options: Vec<OptionRow> = p
            .options
            .iter()
            .map(|o| OptionRow {
                id: o.id.clone(),
                name: o.name.clone(),
                values: o
                    .values
                    .iter()
                    .map(|v| ValueRow {
                        id: v.id.clone(),
                        name: v.name.clone(),
                        code: v.code.clone(),
                        stored: true,
                    })
                    .collect(),
                stored: true,
            })
            .collect();
        let code_of = |id: &str| {
            p.options
                .iter()
                .flat_map(|o| &o.values)
                .find(|v| v.id == id)
                .map(|v| v.code.as_str())
        };
        let combos = p
            .variants
            .iter()
            .map(|v| {
                let codes: Vec<&str> = v
                    .option_value_ids
                    .iter()
                    .filter_map(|id| code_of(id))
                    .collect();
                let default = default_sku(&p.code, &codes);
                Combo {
                    value_ids: v.option_value_ids.clone(),
                    variant_id: v.id.clone(),
                    on: !v.archived,
                    sku: if v.sku == default {
                        String::new()
                    } else {
                        v.sku.clone()
                    },
                    plu: v.plu.map(|n| n.to_string()).unwrap_or_default(),
                    stored: true,
                }
            })
            .collect();
        let base = p.units.iter().find(|u| u.id == p.base_unit_id);
        let base_kind = base
            .and_then(|u| unit_kind(&u.kind).ok())
            .unwrap_or(UnitKind::Integer);
        let mut units: Vec<UnitRow> = Vec::new();
        for u in base
            .into_iter()
            .chain(p.units.iter().filter(|u| u.id != p.base_unit_id))
        {
            units.push(UnitRow {
                id: u.id.clone(),
                name: u.name.clone(),
                short_name: u.short_name.clone(),
                factor: qty_text(u.factor_thousandths),
                sellable: u.sellable,
                purchasable: u.purchasable,
                archived: u.archived,
                stored: true,
            });
        }
        let data = FormData {
            code: p.code.clone(),
            name: p.name.clone(),
            receipt_name: if p.receipt_name == default_receipt_name(&p.name) {
                String::new()
            } else {
                p.receipt_name.clone()
            },
            product_type: product_type(&p.product_type),
            category_id: p.category_id.clone(),
            brand_id: p.brand_id.clone(),
            tax_class_id: p.tax_class_id.clone(),
            tags: p.tags.join(", "),
            open_price: p.open_price,
            requires_note: p.requires_note,
            sellable: p.sellable,
            purchasable: p.purchasable,
            options,
            combos,
            base_kind,
            units,
            default_sale: p.default_sale_unit_id.clone(),
            default_purchase: p.default_purchase_unit_id.clone(),
            barcodes: p
                .barcodes
                .iter()
                .map(|b| BarcodeRow {
                    id: b.id.clone(),
                    variant_id: b.variant_id.clone(),
                    unit_id: b.unit_id.clone(),
                    code: b.code.clone(),
                })
                .collect(),
            components: p
                .components
                .iter()
                .map(|c| ComponentRow {
                    variant_id: c.variant_id.clone(),
                    unit_id: c.unit_id.clone(),
                    qty: qty_text(c.qty_thousandths),
                    product_id: c.product_id.clone(),
                    name: c.name.clone().unwrap_or_default(),
                    sku: c.sku.clone().unwrap_or_default(),
                    units: c
                        .unit_name
                        .clone()
                        .map(|n| vec![(c.unit_id.clone(), n)])
                        .unwrap_or_default(),
                })
                .collect(),
            blocked: blocked.iter().cloned().collect(),
            image: ImageEdit::Keep,
        };
        let mut form = Self {
            id: p.id.clone(),
            is_new: false,
            used: p.used,
            archived: p.archived,
            updated_at: p.updated_at,
            tab: Tab::General,
            errors: Vec::new(),
            has_image,
            baseline: data.clone(),
            data,
        };
        form.regenerate_grid(false);
        form.baseline = form.data.clone();
        form
    }

    /// Whether the editable state differs from the loaded (or last saved) one.
    #[must_use]
    pub fn is_dirty(&self) -> bool {
        self.data != self.baseline
    }

    /// Takes the current state as the clean one (after a save).
    pub fn mark_clean(&mut self) {
        self.baseline = self.data.clone();
    }

    /// The product code as it will be saved (upper case), or empty when it is generated.
    #[must_use]
    pub fn code(&self) -> String {
        self.data.code.trim().to_ascii_uppercase()
    }

    /// The value codes of a combination, upper case, in option order.
    fn value_codes(&self, combo: &Combo) -> Vec<String> {
        combo
            .value_ids
            .iter()
            .filter_map(|id| {
                self.data
                    .options
                    .iter()
                    .flat_map(|o| &o.values)
                    .find(|v| v.id == *id)
                    .map(|v| v.code.trim().to_ascii_uppercase())
            })
            .collect()
    }

    /// The default SKU of combination `i` (`CODE-M-RED`; `…-M-RED` while the code is empty).
    #[must_use]
    pub fn default_sku(&self, i: usize) -> String {
        let Some(combo) = self.data.combos.get(i) else {
            return String::new();
        };
        let codes = self.value_codes(combo);
        let refs: Vec<&str> = codes.iter().map(String::as_str).collect();
        let code = self.code();
        if code.is_empty() {
            default_sku("…", &refs)
        } else {
            default_sku(&code, &refs)
        }
    }

    /// The SKU combination `i` is saved with: the typed one (upper case) or the default; empty
    /// while the product code is still to be generated (the server fills it in).
    #[must_use]
    pub fn effective_sku(&self, i: usize) -> String {
        let Some(combo) = self.data.combos.get(i) else {
            return String::new();
        };
        let typed = combo.sku.trim().to_ascii_uppercase();
        if !typed.is_empty() {
            typed
        } else if self.code().is_empty() {
            String::new()
        } else {
            self.default_sku(i)
        }
    }

    /// The names of a combination's values, e.g. `Medium / Red`.
    #[must_use]
    pub fn combo_label(&self, combo: &Combo) -> String {
        let names: Vec<&str> = combo
            .value_ids
            .iter()
            .filter_map(|id| {
                self.data
                    .options
                    .iter()
                    .flat_map(|o| &o.values)
                    .find(|v| v.id == *id)
                    .map(|v| v.name.as_str())
            })
            .collect();
        names.join(" / ")
    }

    /// Rebuilds the grid from the options, keeping known combinations. New combinations start
    /// `on` (`new_on`) or off (a loaded product's combinations it never had).
    pub fn regenerate_grid(&mut self, new_on: bool) {
        let mut grid: Vec<Vec<String>> = vec![Vec::new()];
        for option in &self.data.options {
            let mut next = Vec::new();
            for prefix in &grid {
                for value in &option.values {
                    let mut combo = prefix.clone();
                    combo.push(value.id.clone());
                    next.push(combo);
                }
            }
            grid = next;
        }
        let old = std::mem::take(&mut self.data.combos);
        self.data.combos = grid
            .into_iter()
            .map(|value_ids| {
                old.iter()
                    .find(|c| c.value_ids == value_ids)
                    .cloned()
                    .unwrap_or_else(|| Combo {
                        value_ids,
                        variant_id: new_id(),
                        on: new_on,
                        sku: String::new(),
                        plu: String::new(),
                        stored: false,
                    })
            })
            .collect();
    }

    /// Whether options can be added or removed (not after use, not for a bundle, at most 3).
    #[must_use]
    pub fn can_add_option(&self) -> bool {
        !self.used
            && self.data.product_type != ProductType::Bundle
            && self.data.options.len() < MAX_OPTIONS
    }

    /// Adds an empty option with one empty value.
    pub fn add_option(&mut self) {
        if !self.can_add_option() {
            return;
        }
        self.data.options.push(OptionRow {
            id: new_id(),
            name: String::new(),
            values: vec![ValueRow {
                id: new_id(),
                name: String::new(),
                code: String::new(),
                stored: false,
            }],
            stored: false,
        });
        self.regenerate_grid(true);
    }

    /// Removes option `i` (not after use).
    pub fn remove_option(&mut self, i: usize) {
        if self.used || i >= self.data.options.len() {
            return;
        }
        self.data.options.remove(i);
        self.regenerate_grid(true);
    }

    /// Adds a value to option `i` (allowed after use too).
    pub fn add_value(&mut self, i: usize) {
        let Some(option) = self.data.options.get_mut(i) else {
            return;
        };
        option.values.push(ValueRow {
            id: new_id(),
            name: String::new(),
            code: String::new(),
            stored: false,
        });
        self.regenerate_grid(true);
    }

    /// Whether value `j` of option `i` can be removed (a stored value of a used product cannot:
    /// archive its variants instead).
    #[must_use]
    pub fn can_remove_value(&self, i: usize, j: usize) -> bool {
        self.data
            .options
            .get(i)
            .and_then(|o| o.values.get(j))
            .is_some_and(|v| !(self.used && v.stored))
    }

    /// Removes value `j` of option `i`; an option needs at least one value.
    pub fn remove_value(&mut self, i: usize, j: usize) {
        if !self.can_remove_value(i, j) || self.data.options[i].values.len() < 2 {
            return;
        }
        self.data.options[i].values.remove(j);
        self.regenerate_grid(true);
    }

    /// Switches combination `i` on or off.
    pub fn set_combo_on(&mut self, i: usize, on: bool) {
        if let Some(c) = self.data.combos.get_mut(i) {
            c.on = on;
        }
    }

    /// Changes the type: a weighed product gets a decimal `kg` base unit and no other units, a
    /// bundle loses its options and other units and becomes integer; only a bundle keeps
    /// components; purchasable follows the type's default (spec 2.2 §2, §5). Not after use.
    pub fn set_type(&mut self, t: ProductType) {
        if self.used || self.data.product_type == t {
            return;
        }
        self.data.product_type = t;
        self.data.purchasable = t.default_purchasable();
        match t {
            ProductType::Weighed => {
                self.data.base_kind = UnitKind::Decimal;
                self.data.units.truncate(1);
                if self.data.units[0].name == "pcs" {
                    "kg".clone_into(&mut self.data.units[0].name);
                }
            }
            ProductType::Bundle => {
                self.data.base_kind = UnitKind::Integer;
                self.data.units.truncate(1);
                self.data.options.clear();
                self.regenerate_grid(true);
            }
            ProductType::Stock | ProductType::Service => {}
        }
        if t != ProductType::Bundle {
            self.data.components.clear();
        }
        if t != ProductType::Weighed {
            for c in &mut self.data.combos {
                c.plu.clear();
            }
        }
        let base = self.data.units[0].id.clone();
        let ids: Vec<&String> = self.data.units.iter().map(|u| &u.id).collect();
        if !ids.contains(&&self.data.default_sale) {
            self.data.default_sale.clone_from(&base);
        }
        if !ids.contains(&&self.data.default_purchase) {
            self.data.default_purchase = base;
        }
    }

    /// Whether another unit can be added (stock and service only, at most 5 active extras).
    #[must_use]
    pub fn can_add_unit(&self) -> bool {
        matches!(
            self.data.product_type,
            ProductType::Stock | ProductType::Service
        ) && self
            .data
            .units
            .iter()
            .skip(1)
            .filter(|u| !u.archived)
            .count()
            < MAX_EXTRA_UNITS
    }

    /// Adds an extra unit row.
    pub fn add_unit(&mut self) {
        if !self.can_add_unit() {
            return;
        }
        self.data.units.push(UnitRow {
            id: new_id(),
            name: String::new(),
            short_name: String::new(),
            factor: String::new(),
            sellable: true,
            purchasable: true,
            archived: false,
            stored: false,
        });
    }

    /// Removes extra unit `i` (≥ 1); a stored unit of a used product is archived instead, and
    /// its barcodes go with it otherwise.
    pub fn remove_unit(&mut self, i: usize) {
        if i == 0 || i >= self.data.units.len() {
            return;
        }
        if self.used && self.data.units[i].stored {
            let archived = !self.data.units[i].archived;
            self.data.units[i].archived = archived;
            return;
        }
        let id = self.data.units.remove(i).id;
        self.data.barcodes.retain(|b| b.unit_id != id);
        let base = self.data.units[0].id.clone();
        if self.data.default_sale == id {
            self.data.default_sale.clone_from(&base);
        }
        if self.data.default_purchase == id {
            self.data.default_purchase = base;
        }
    }

    /// Whether unit row `i`'s factor is read-only (the base unit, or stored after use).
    #[must_use]
    pub fn factor_locked(&self, i: usize) -> bool {
        i == 0 || (self.used && self.data.units.get(i).is_some_and(|u| u.stored))
    }

    /// Active units `(id, name)` in row order (for pickers).
    #[must_use]
    pub fn active_units(&self) -> Vec<(String, String)> {
        self.data
            .units
            .iter()
            .filter(|u| !u.archived)
            .map(|u| (u.id.clone(), u.name.clone()))
            .collect()
    }

    /// Indices of the combinations that are on, in grid order.
    #[must_use]
    pub fn active_combos(&self) -> Vec<usize> {
        (0..self.data.combos.len())
            .filter(|i| self.data.combos[*i].on)
            .collect()
    }

    /// The display name of a variant (`Kaos – M / Red`, or the product name).
    #[must_use]
    pub fn variant_label(&self, variant_id: &str) -> String {
        let name = if self.data.name.trim().is_empty() {
            "…".to_owned()
        } else {
            self.data.name.trim().to_owned()
        };
        self.data
            .combos
            .iter()
            .find(|c| c.variant_id == variant_id)
            .map(|c| {
                let values = self.combo_label(c);
                if values.is_empty() {
                    name.clone()
                } else {
                    format!("{name} – {values}")
                }
            })
            .unwrap_or_default()
    }

    /// Adds a barcode for a variant + unit.
    pub fn add_barcode(&mut self, variant_id: &str, unit_id: &str, code: &str) {
        self.data.barcodes.push(BarcodeRow {
            id: new_id(),
            variant_id: variant_id.to_owned(),
            unit_id: unit_id.to_owned(),
            code: code.trim().to_owned(),
        });
    }

    /// Adds a bundle component (qty 1) unless the variant + unit is already a line.
    pub fn add_component(&mut self, row: ComponentRow) {
        if self
            .data
            .components
            .iter()
            .any(|c| c.variant_id == row.variant_id && c.unit_id == row.unit_id)
        {
            return;
        }
        self.data.components.push(row);
    }

    /// Fills the unit choices of the components of `product_id` (from its loaded document).
    pub fn set_component_units(&mut self, product_id: &str, units: &[(String, String)]) {
        for c in &mut self.data.components {
            if c.product_id.as_deref() == Some(product_id) {
                c.units = units.to_vec();
            }
        }
    }

    /// The document to `PUT`. Typed numbers that do not parse become 0 (`validate` reports
    /// them first).
    #[must_use]
    #[allow(clippy::too_many_lines)] // one block per child list
    pub fn to_body(&self) -> Product {
        let d = &self.data;
        let kind = unit_kind_str(d.base_kind).to_owned();
        let units = d
            .units
            .iter()
            .enumerate()
            .map(|(i, u)| Unit {
                id: u.id.clone(),
                name: u.name.trim().to_owned(),
                short_name: u.short_name.trim().to_owned(),
                kind: kind.clone(),
                factor_thousandths: if i == 0 {
                    1000
                } else {
                    Qty::parse(u.factor.trim()).map_or(0, Qty::thousandths)
                },
                sellable: u.sellable,
                purchasable: u.purchasable,
                archived: u.archived,
            })
            .collect();
        let weighed = d.product_type == ProductType::Weighed;
        let variants = d
            .combos
            .iter()
            .enumerate()
            .filter(|(_, c)| c.on || c.stored)
            .map(|(i, c)| Variant {
                id: c.variant_id.clone(),
                sku: self.effective_sku(i),
                option_value_ids: c.value_ids.clone(),
                plu: if weighed {
                    c.plu.trim().parse().ok()
                } else {
                    None
                },
                archived: !c.on,
            })
            .collect();
        Product {
            id: self.id.clone(),
            code: self.code(),
            name: d.name.trim().to_owned(),
            receipt_name: d.receipt_name.trim().to_owned(),
            product_type: d.product_type.as_str().to_owned(),
            tracking: "none".to_owned(),
            category_id: d.category_id.clone(),
            brand_id: d.brand_id.clone(),
            tax_class_id: d.tax_class_id.clone(),
            tags: d
                .tags
                .split(',')
                .map(str::trim)
                .filter(|t| !t.is_empty())
                .map(str::to_owned)
                .collect(),
            sellable: d.sellable,
            purchasable: d.purchasable,
            open_price: d.open_price,
            requires_note: d.requires_note,
            options: d
                .options
                .iter()
                .map(|o| ProductOption {
                    id: o.id.clone(),
                    name: o.name.trim().to_owned(),
                    values: o
                        .values
                        .iter()
                        .map(|v| OptionValue {
                            id: v.id.clone(),
                            name: v.name.trim().to_owned(),
                            code: v.code.trim().to_ascii_uppercase(),
                        })
                        .collect(),
                })
                .collect(),
            units,
            base_unit_id: d.units[0].id.clone(),
            default_sale_unit_id: d.default_sale.clone(),
            default_purchase_unit_id: d.default_purchase.clone(),
            variants,
            barcodes: d
                .barcodes
                .iter()
                .map(|b| Barcode {
                    id: b.id.clone(),
                    variant_id: b.variant_id.clone(),
                    unit_id: b.unit_id.clone(),
                    code: b.code.trim().to_owned(),
                    allow_invalid_check_digit: false,
                })
                .collect(),
            archived_barcodes: Vec::new(),
            components: d
                .components
                .iter()
                .map(|c| Component {
                    variant_id: c.variant_id.clone(),
                    unit_id: c.unit_id.clone(),
                    qty_thousandths: Qty::parse(c.qty.trim()).map_or(0, Qty::thousandths),
                    product_id: None,
                    name: None,
                    sku: None,
                    unit_name: None,
                })
                .collect(),
            archived: self.archived,
            archived_at: None,
            used: self.used,
            created_at: 0,
            updated_at: 0,
        }
    }

    /// Checks the typed numbers and the core rules; returns whether the form may be saved. The
    /// errors are in [`ProductForm::errors`] and the tab switches to the first one.
    pub fn validate(&mut self) -> bool {
        let mut errors = self.number_errors();
        let mut body = self.to_body();
        if body.code.is_empty() {
            // Generated on save; check the rest with a stand-in code and SKUs.
            "GENERATED".clone_into(&mut body.code);
            for (v, c) in body
                .variants
                .iter_mut()
                .zip(self.data.combos.iter().filter(|c| c.on || c.stored))
            {
                if v.sku.is_empty() {
                    let codes = self.value_codes(c);
                    let refs: Vec<&str> = codes.iter().map(String::as_str).collect();
                    v.sku = default_sku("GENERATED", &refs);
                }
            }
        }
        match body.to_draft(&self.id) {
            Ok(draft) => {
                if let Err(rules) = validate_product(&draft.normalized()) {
                    for e in &rules {
                        let error = self.form_error(e);
                        if !errors
                            .iter()
                            .any(|x| x.field == error.field && x.row == error.row)
                        {
                            errors.push(error);
                        }
                    }
                }
            }
            Err(problem) => errors.extend(self.problem_errors(&problem)),
        }
        self.set_errors(errors);
        self.errors.is_empty()
    }

    fn number_errors(&self) -> Vec<FormError> {
        let mut errors = Vec::new();
        for (i, u) in self.data.units.iter().enumerate().skip(1) {
            match Qty::parse(u.factor.trim()) {
                Err(_) => errors.push(FormError::new(
                    "units.factor_thousandths",
                    Some(i),
                    "invalid_number",
                    u.factor.trim(),
                )),
                Ok(q) if self.data.base_kind == UnitKind::Integer && !q.is_whole() => {
                    errors.push(FormError::new(
                        "units.factor_thousandths",
                        Some(i),
                        "factor_whole",
                        u.name.trim(),
                    ));
                }
                Ok(q) if q.thousandths() <= 1000 => errors.push(FormError::new(
                    "units.factor_thousandths",
                    Some(i),
                    "factor_above_one",
                    u.name.trim(),
                )),
                Ok(_) => {}
            }
        }
        if self.data.product_type == ProductType::Weighed {
            for (i, c) in self.data.combos.iter().enumerate() {
                let plu = c.plu.trim();
                if !plu.is_empty()
                    && (plu.len() > 6
                        || !plu.bytes().all(|b| b.is_ascii_digit())
                        || plu.bytes().all(|b| b == b'0'))
                {
                    errors.push(FormError::new("variants.plu", Some(i), "invalid_plu", plu));
                }
            }
        }
        for (i, c) in self.data.components.iter().enumerate() {
            match Qty::parse(c.qty.trim()) {
                Ok(q) if q.is_positive() => {}
                _ => errors.push(FormError::new(
                    "components.qty_thousandths",
                    Some(i),
                    "invalid_number",
                    c.qty.trim(),
                )),
            }
        }
        errors
    }

    /// Replaces the errors and moves to the first tab (in tab order) that has one.
    pub fn set_errors(&mut self, errors: Vec<FormError>) {
        if let Some(first) = errors.iter().map(|e| e.tab).min() {
            self.tab = first;
        }
        self.errors = errors;
    }

    /// The row a value names, for `field`.
    fn row_of(&self, field: &str, value: &str) -> Option<usize> {
        let value_upper = value.to_ascii_uppercase();
        match field {
            "variants.sku" => (0..self.data.combos.len())
                .find(|i| self.data.combos[*i].on && self.effective_sku(*i) == value_upper),
            "variants.plu" => self.data.combos.iter().position(|c| c.plu.trim() == value),
            "barcodes.code" => self
                .data
                .barcodes
                .iter()
                .position(|b| b.code.trim() == value),
            "units.name" | "units.factor_thousandths" => {
                self.data.units.iter().position(|u| u.name.trim() == value)
            }
            "options.values.code" | "options" => self.data.options.iter().position(|o| {
                o.name.trim() == value
                    || o.values
                        .iter()
                        .any(|v| v.code.trim().eq_ignore_ascii_case(value))
            }),
            _ => None,
        }
    }

    /// A core rule failure on the form.
    fn form_error(&self, e: &CatalogError) -> FormError {
        let field = e.field().unwrap_or("name");
        let (code, arg) = match e {
            CatalogError::InvalidField { field, .. } => (
                format!("invalid_{}", field.replace('.', "_")),
                String::new(),
            ),
            CatalogError::InvalidCode { kind, value } => (
                match kind {
                    CodeKind::ProductCode => "invalid_product_code",
                    CodeKind::Sku => "invalid_sku",
                    CodeKind::OptionValueCode => "invalid_value_code",
                    CodeKind::Barcode => "invalid_barcode",
                    _ => "invalid_code",
                }
                .to_owned(),
                value.clone(),
            ),
            CatalogError::ProductTypeRule { reason, .. } => {
                (type_rule_code(reason).to_owned(), String::new())
            }
            CatalogError::InvalidUnitFactor { unit, .. } => {
                ("invalid_unit_factor".to_owned(), unit.clone())
            }
            CatalogError::DuplicateUnitName { name } => {
                ("duplicate_unit_name".to_owned(), name.clone())
            }
            CatalogError::DuplicateOption { name } => ("duplicate_option".to_owned(), name.clone()),
            CatalogError::DuplicateOptionValue { value, .. } => {
                ("duplicate_option_value".to_owned(), value.clone())
            }
            CatalogError::InvalidCheckDigit { code } => {
                ("invalid_check_digit".to_owned(), code.clone())
            }
            other => (other.code().to_owned(), String::new()),
        };
        let row = match e {
            CatalogError::InvalidCode { value, .. } => self.row_of(field, value),
            CatalogError::InvalidCheckDigit { code } => self.row_of(field, code),
            CatalogError::InvalidUnitFactor { unit, .. } => self.row_of(field, unit),
            CatalogError::DuplicateUnitName { name } => self.row_of(field, name),
            CatalogError::DuplicateOption { name } => self.row_of("options", name),
            CatalogError::DuplicateOptionValue { option, .. } => self.row_of("options", option),
            _ => None,
        };
        FormError::new(field, row, &code, arg)
    }

    /// The server's failures (`422 errors[]`, `409 duplicate_*`, `400 invalid_body`) on the
    /// form.
    #[must_use]
    pub fn problem_errors(&self, problem: &Problem) -> Vec<FormError> {
        if let Some(dup) = problem.duplicate() {
            let field = problem
                .field()
                .unwrap_or(if problem.code == "duplicate_code" {
                    "code"
                } else {
                    "variants.sku"
                });
            let row = self.row_of(field, &dup.value);
            return vec![FormError::new(field, row, &problem.code, dup.value)];
        }
        problem
            .errors()
            .into_iter()
            .map(
                |FieldError {
                     code,
                     field,
                     message,
                 }| {
                    let field = field.unwrap_or_else(|| {
                        if code == "invalid_reference" {
                            "category_id"
                        } else {
                            "name"
                        }
                        .to_owned()
                    });
                    let value = backticked(&message).unwrap_or_default().to_owned();
                    let row = self.row_of(&field, &value);
                    let code = if code == "product_type_rule" {
                        type_rule_code(&message).to_owned()
                    } else if code == "frozen_after_use" {
                        format!("frozen_{}", field.replace('.', "_"))
                    } else if code == "invalid_field" {
                        format!("invalid_{}", field.replace('.', "_"))
                    } else {
                        code
                    };
                    FormError::new(&field, row, &code, value)
                },
            )
            .collect()
    }

    /// Shows a server failure on the form; returns whether it was a field error.
    pub fn apply_problem(&mut self, problem: &Problem) -> bool {
        if !(problem.status == 422
            || problem.code.starts_with("duplicate_")
            || problem.code == "invalid_body")
        {
            return false;
        }
        let errors = self.problem_errors(problem);
        self.set_errors(errors);
        true
    }

    /// The first error for `field` (and `row`).
    #[must_use]
    pub fn error_for(&self, field: &str, row: Option<usize>) -> Option<&FormError> {
        self.errors.iter().find(|e| e.is_for(field, row))
    }

    /// Whether a tab has errors (for the dot on the tab).
    #[must_use]
    pub fn tab_has_errors(&self, tab: Tab) -> bool {
        self.errors.iter().any(|e| e.tab == tab)
    }

    /// The default receipt name shown as a placeholder.
    #[must_use]
    pub fn receipt_placeholder(&self) -> String {
        default_receipt_name(&self.data.name)
    }
}

#[cfg(test)]
mod tests;
