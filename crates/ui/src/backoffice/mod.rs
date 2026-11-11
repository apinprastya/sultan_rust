//! The back office's screen flow as a plain state machine (phase C4, spec 2.2 §2–§12), like the
//! register's [`crate::controller`]: [`Controller::update`] takes a [`Msg`] (a key, a click, an
//! edit, or the result of a call) and returns the [`Effect`]s to run through
//! [`sultan_client::BackOffice`]. No Slint and no IO here (`crate::backoffice_app` is the glue).
//!
//! Screens ([`Screen`]):
//! 1. **Login** — username + password (`POST /auth/password`), then `GET /auth/me`. Without
//!    `catalog.manage` (from any role assignment, spec 2.2 §11) the catalog is read-only and a
//!    banner says why; browsing still works.
//! 2. **Products** — search (debounced, `q`), filters category / brand / type and, with
//!    `catalog.view_archived`, archived; pages of 50 with `before_id` ("load more", and moving
//!    past the last row loads the next page). `/` or Ctrl+F search, ↑/↓ move, Enter open,
//!    Ctrl+N new, F5 refresh, Ctrl+Shift+D deletes the highlighted product (asking first; only a
//!    never-used one, else `in_use` says to archive it, spec 2.2 §9).
//! 3. **Product form** — [`form::ProductForm`]: tabs General / Variants / Units & barcodes
//!    (Ctrl+1/2/3). Ctrl+S checks the core rules, then `PUT /products/{id}`, then the branch
//!    blocks and the image; `422 errors[]` and `409 duplicate_*` go to their field and tab. Esc
//!    goes back (asking first when there are unsaved changes); Ctrl+Shift+A archives or
//!    unarchives (asking first).
//! 4. **Categories** — the tree: add root / child, rename, move (parent), archive, delete
//!    (Ctrl+Shift+D, asking first; only without products or children).
//! 5. **Brands** — add, rename, archive, delete (Ctrl+Shift+D, asking first; only without
//!    products).
//!
//! F2 / F3 / F4 switch between products, categories and brands; F12 logs out.
//!
//! **Live refresh**: after login the glue watches `GET /events`; `catalog.changed` reloads the
//! product list (or the taxonomy) and, for the product open in the form, fetches it: when it
//! changed elsewhere the form shows a warning with "reload".

pub mod form;

use std::collections::BTreeSet;

use sultan_client::catalog::{CATALOG_MANAGE, CATALOG_VIEW_ARCHIVED};
use sultan_client::{
    Branch, Brand, BrandBody, CatalogSettings, Category, CategoryBody, CategoryNode, ClientError,
    Generate, ImageInfo, Login, Me, Product, ProductPage, ProductQuery, ProductSummary, SearchHit,
    ServerEvent, TaxClass,
};
use sultan_core::catalog::ProductType;

use crate::controller::{Notice, Tone};
use form::{ComponentRow, ImageEdit, ProductForm, Tab, new_id};

/// Rows per page of the product list.
pub const PAGE_SIZE: u32 = 50;
/// Hits of the component search.
pub const SEARCH_LIMIT: u32 = 10;
/// Product types in picker order.
pub const TYPES: [ProductType; 4] = [
    ProductType::Stock,
    ProductType::Weighed,
    ProductType::Service,
    ProductType::Bundle,
];

/// A key press: the key and its modifiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Press {
    /// The key.
    pub key: PressKey,
    /// Ctrl held.
    pub ctrl: bool,
    /// Shift held.
    pub shift: bool,
}

/// The keys the back office reacts to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PressKey {
    /// A printable character, lower case.
    Char(char),
    /// Enter.
    Enter,
    /// Escape.
    Escape,
    /// Arrow up.
    Up,
    /// Arrow down.
    Down,
    /// F1–F12.
    F(u8),
    /// Anything else.
    Other,
}

impl Press {
    /// The press for a Slint key event text and its modifiers.
    #[must_use]
    pub fn parse(text: &str, ctrl: bool, shift: bool) -> Self {
        let mut chars = text.chars();
        let key = match (chars.next(), chars.next()) {
            (Some(c), None) => match c {
                '\n' | '\r' => PressKey::Enter,
                '\u{1b}' => PressKey::Escape,
                '\u{F700}' => PressKey::Up,
                '\u{F701}' => PressKey::Down,
                c if (0xF704..0xF704 + 12).contains(&u32::from(c)) => {
                    PressKey::F(u8::try_from(u32::from(c) - 0xF704 + 1).unwrap_or(0))
                }
                c if !c.is_control() && u32::from(c) < 0xF700 => {
                    PressKey::Char(c.to_lowercase().next().unwrap_or(c))
                }
                _ => PressKey::Other,
            },
            _ => PressKey::Other,
        };
        Self { key, ctrl, shift }
    }

    const fn ctrl_char(self, c: char) -> bool {
        self.ctrl && !self.shift && matches!(self.key, PressKey::Char(k) if k == c)
    }

    const fn ctrl_shift_char(self, c: char) -> bool {
        self.ctrl && self.shift && matches!(self.key, PressKey::Char(k) if k == c)
    }

    const fn plain(self, key: PressKey) -> bool {
        !self.ctrl
            && matches!(
                (self.key, key),
                (PressKey::Enter, PressKey::Enter)
                    | (PressKey::Escape, PressKey::Escape)
                    | (PressKey::Up, PressKey::Up)
                    | (PressKey::Down, PressKey::Down)
            )
    }

    const fn f(self, n: u8) -> bool {
        matches!(self.key, PressKey::F(k) if k == n)
    }
}

/// The screens; see the module docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    /// Password login.
    Login,
    /// Product list.
    Products,
    /// Product form.
    Form,
    /// Category tree.
    Categories,
    /// Brand list.
    Brands,
}

impl Screen {
    /// The name the view switches on.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Login => "login",
            Self::Products => "products",
            Self::Form => "form",
            Self::Categories => "categories",
            Self::Brands => "brands",
        }
    }
}

/// What the confirmation dialog asks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Confirm {
    /// Leave the form and drop the unsaved changes; then go to this screen.
    Discard(Screen),
    /// Archive (or unarchive) the open product.
    Archive,
    /// Delete [`Controller::deleting`] (a never-used product, category or brand; spec 2.2 §9).
    Delete(Deletable),
}

/// What [`Confirm::Delete`] deletes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Deletable {
    /// The highlighted product of the list.
    Product,
    /// The category in the editor.
    Category,
    /// The brand in the editor.
    Brand,
}

/// The login screen.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LoginState {
    /// Username as typed.
    pub username: String,
    /// Password as typed (never shown).
    pub password: String,
    /// Wrong password, network…
    pub notice: Option<Notice>,
}

/// The logged-in user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    /// Bearer token.
    pub token: String,
    /// `GET /auth/me` (empty until it arrives).
    pub me: Me,
    /// Display name from the login.
    pub display_name: String,
}

impl Session {
    /// `catalog.manage` through any role assignment (spec 2.2 §11).
    #[must_use]
    pub fn can_manage(&self) -> bool {
        self.me.holds_anywhere(CATALOG_MANAGE)
    }

    /// `catalog.view_archived` through any role assignment.
    #[must_use]
    pub fn can_view_archived(&self) -> bool {
        self.me.holds_anywhere(CATALOG_VIEW_ARCHIVED)
    }

    /// Why the catalog is read-only, if it is: `read_only` without `catalog.manage`.
    #[must_use]
    pub fn read_only_reason(&self) -> Option<&'static str> {
        if self.me.user_id.is_empty() || self.can_manage() {
            None
        } else {
            Some("read_only")
        }
    }
}

/// A category in tree order with its depth (0 = root).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CategoryRow {
    /// The category.
    pub category: Category,
    /// Depth.
    pub depth: usize,
}

fn flatten(nodes: &[CategoryNode], depth: usize, out: &mut Vec<CategoryRow>) {
    for n in nodes {
        out.push(CategoryRow {
            category: n.category(),
            depth,
        });
        flatten(&n.children, depth + 1, out);
    }
}

/// Reference data for pickers and list columns.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Reference {
    /// Tax classes.
    pub tax_classes: Vec<TaxClass>,
    /// Categories in tree order (archived ones only with `catalog.view_archived`).
    pub categories: Vec<CategoryRow>,
    /// Brands by name.
    pub brands: Vec<Brand>,
    /// Branches (for the branch blocks).
    pub branches: Vec<Branch>,
    /// Catalog settings.
    pub settings: CatalogSettings,
}

impl Reference {
    /// A category's name.
    #[must_use]
    pub fn category_name(&self, id: Option<&str>) -> String {
        id.and_then(|id| self.categories.iter().find(|c| c.category.id == id))
            .map(|c| c.category.name.clone())
            .unwrap_or_default()
    }

    /// A brand's name.
    #[must_use]
    pub fn brand_name(&self, id: Option<&str>) -> String {
        id.and_then(|id| self.brands.iter().find(|b| b.id == id))
            .map(|b| b.name.clone())
            .unwrap_or_default()
    }

    /// Category choices: `None` (no category) first, then active categories indented by depth
    /// (`keep`, the product's own, is kept even when archived).
    #[must_use]
    pub fn category_choices(&self, keep: Option<&str>) -> Vec<(Option<String>, String)> {
        let mut out = vec![(None, String::new())];
        for row in &self.categories {
            if row.category.archived && keep != Some(row.category.id.as_str()) {
                continue;
            }
            let label = format!("{}{}", "  ".repeat(row.depth), row.category.name);
            out.push((Some(row.category.id.clone()), label));
        }
        out
    }

    /// Brand choices: `None` first, then active brands (and `keep`).
    #[must_use]
    pub fn brand_choices(&self, keep: Option<&str>) -> Vec<(Option<String>, String)> {
        let mut out = vec![(None, String::new())];
        for b in &self.brands {
            if !b.archived || keep == Some(b.id.as_str()) {
                out.push((Some(b.id.clone()), b.name.clone()));
            }
        }
        out
    }
}

/// The product list.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ListState {
    /// Search text as typed.
    pub search: String,
    /// Bumped per keystroke; the debounce only runs the latest.
    pub search_serial: u64,
    /// Category filter.
    pub category: Option<String>,
    /// Brand filter.
    pub brand: Option<String>,
    /// Type filter.
    pub product_type: Option<ProductType>,
    /// Archived products only.
    pub archived: bool,
    /// Loaded rows.
    pub rows: Vec<ProductSummary>,
    /// Cursor of the next page.
    pub next: Option<String>,
    /// The highlighted row.
    pub cursor: usize,
    /// A page is loading.
    pub loading: bool,
    /// Errors, or "deleted" (kept while the list reloads, until the next message).
    pub notice: Option<Notice>,
}

impl ListState {
    /// The query of the first page (or the next one with `before_id`).
    #[must_use]
    pub fn query(&self, before_id: Option<String>) -> ProductQuery {
        ProductQuery {
            q: Some(self.search.trim().to_owned()).filter(|q| !q.is_empty()),
            category_id: self.category.clone(),
            brand_id: self.brand.clone(),
            product_type: self.product_type.map(|t| t.as_str().to_owned()),
            archived: self.archived,
            before_id,
            limit: Some(PAGE_SIZE),
        }
    }
}

/// The open product form and what goes with it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormState {
    /// The form.
    pub form: ProductForm,
    /// Saved, errors, permission…
    pub notice: Option<Notice>,
    /// The product was changed elsewhere since it was loaded.
    pub changed_elsewhere: bool,
    /// The image shown (stored or picked), for the thumbnail.
    pub image: Option<Vec<u8>>,
    /// Bumped when [`FormState::image`] changes.
    pub image_serial: u64,
    /// Path typed for an image to upload.
    pub image_path: String,
    /// Component search text.
    pub component_search: String,
    /// Component search hits.
    pub hits: Vec<SearchHit>,
    /// Barcode picker: index into the active combinations.
    pub pick_variant: usize,
    /// Barcode picker: index into the active units.
    pub pick_unit: usize,
    /// Barcode picker: the code typed.
    pub new_barcode: String,
    /// A save is running (product, then blocks and image).
    pub saving: bool,
    /// Calls of the save still outstanding after the product itself.
    pub outstanding: usize,
    /// What a running generator call fills in.
    pub generating: Option<Generate>,
    /// Blocks as stored (to know whether to `PUT` them).
    pub stored_blocks: BTreeSet<String>,
}

impl FormState {
    fn new(form: ProductForm) -> Self {
        Self {
            stored_blocks: form.data.blocked.clone(),
            form,
            notice: None,
            changed_elsewhere: false,
            image: None,
            image_serial: 0,
            image_path: String::new(),
            component_search: String::new(),
            hits: Vec::new(),
            pick_variant: 0,
            pick_unit: 0,
            new_barcode: String::new(),
            saving: false,
            outstanding: 0,
            generating: None,
        }
    }
}

/// The category editor.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CategoryEditor {
    /// The highlighted row (into [`Reference::categories`]).
    pub cursor: usize,
    /// The category being edited, `None` for a new one.
    pub editing: Option<String>,
    /// Name as typed.
    pub name: String,
    /// Short code as typed.
    pub code: String,
    /// Parent.
    pub parent: Option<String>,
    /// Saved, errors.
    pub notice: Option<Notice>,
}

/// The brand list.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BrandEditor {
    /// The highlighted row.
    pub cursor: usize,
    /// The brand being edited, `None` for a new one.
    pub editing: Option<String>,
    /// Name as typed.
    pub name: String,
    /// Saved, errors.
    pub notice: Option<Notice>,
}

/// A service call to make.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    /// `password_login`.
    Login {
        /// Username.
        username: String,
        /// Password.
        password: String,
    },
    /// `me`.
    LoadMe(String),
    /// `tax_classes`.
    LoadTaxClasses(String),
    /// `categories`.
    LoadCategories {
        /// Token.
        token: String,
        /// With archived ones.
        include_archived: bool,
    },
    /// `brands`.
    LoadBrands {
        /// Token.
        token: String,
        /// With archived ones.
        include_archived: bool,
    },
    /// `branches`.
    LoadBranches(String),
    /// `catalog_settings`.
    LoadSettings(String),
    /// `products`; the result is appended when `append`.
    LoadProducts {
        /// Token.
        token: String,
        /// Query.
        query: ProductQuery,
        /// Next page.
        append: bool,
    },
    /// `product` for the form.
    LoadProduct {
        /// Token.
        token: String,
        /// Product.
        id: String,
    },
    /// `branch_blocks`.
    LoadBlocks {
        /// Token.
        token: String,
        /// Product.
        id: String,
    },
    /// `product_image`.
    LoadImage {
        /// Token.
        token: String,
        /// Product.
        id: String,
    },
    /// `put_product`.
    SaveProduct {
        /// Token.
        token: String,
        /// Product.
        id: String,
        /// Document.
        body: Box<Product>,
    },
    /// `put_branch_blocks`.
    PutBlocks {
        /// Token.
        token: String,
        /// Product.
        id: String,
        /// Blocked branches.
        branch_ids: Vec<String>,
    },
    /// `put_product_image`.
    PutImage {
        /// Token.
        token: String,
        /// Product.
        id: String,
        /// Bytes.
        bytes: Vec<u8>,
    },
    /// `delete_product_image`.
    DeleteImage {
        /// Token.
        token: String,
        /// Product.
        id: String,
    },
    /// `set_archived`.
    SetArchived {
        /// Token.
        token: String,
        /// Product.
        id: String,
        /// Archive (true) or unarchive.
        archived: bool,
        /// Idempotency key.
        key: String,
    },
    /// `generate`.
    Generate {
        /// Token.
        token: String,
        /// What.
        what: Generate,
        /// Idempotency key.
        key: String,
    },
    /// `search` for bundle components.
    Search {
        /// Token.
        token: String,
        /// Text.
        q: String,
    },
    /// `product` of a bundle component (its units).
    LoadComponent {
        /// Token.
        token: String,
        /// Product.
        id: String,
    },
    /// `product` again, to see whether it changed elsewhere.
    CheckProduct {
        /// Token.
        token: String,
        /// Product.
        id: String,
    },
    /// `put_category`.
    PutCategory {
        /// Token.
        token: String,
        /// Category.
        id: String,
        /// Body.
        body: CategoryBody,
    },
    /// `put_brand`.
    PutBrand {
        /// Token.
        token: String,
        /// Brand.
        id: String,
        /// Body.
        body: BrandBody,
    },
    /// `delete_product`, `delete_category` or `delete_brand`.
    Delete {
        /// Token.
        token: String,
        /// What.
        what: Deletable,
        /// Its ID.
        id: String,
    },
    /// `logout`.
    Logout(String),
    /// `watch_events`; events come back as [`Msg::Event`] with `generation`.
    WatchEvents {
        /// Token.
        token: String,
        /// The login they belong to.
        generation: u64,
    },
    /// Wait a moment, then send [`Msg::SearchDue`] with `serial` (debounce).
    Debounce(u64),
    /// Read a file (the image to upload) and send [`Msg::FileRead`].
    ReadFile(String),
    /// Close the window: back to the Start screen (F9 on the login screen).
    Exit,
}

/// Input for [`Controller::update`].
#[derive(Debug, Clone, PartialEq)]
pub enum Msg {
    /// A key, before the focused widget sees it (capture phase).
    Capture(Press),
    /// A key no widget used (bubble phase).
    Key(Press),
    /// A button.
    Action(String),
    /// A text field changed: `kind` (`login`, `list`, `form`, `option`, `value`, `combo`,
    /// `unit`, `barcode`, `component`, `category`, `brand`), row, sub-row, field, text.
    Edit {
        /// What is edited.
        kind: String,
        /// Row.
        row: usize,
        /// Sub-row (option values).
        sub: usize,
        /// Field.
        field: String,
        /// Text.
        value: String,
    },
    /// A check box changed.
    Toggle {
        /// What.
        kind: String,
        /// Row.
        row: usize,
        /// Field.
        field: String,
        /// Checked.
        on: bool,
    },
    /// A choice in a picker: `kind`, row, field, index into the choices the view shows.
    Choose {
        /// What.
        kind: String,
        /// Row.
        row: usize,
        /// Field.
        field: String,
        /// Index.
        index: usize,
    },
    /// A click on a row: `products` (select), `open` (open), `category`, `brand`, `hit`,
    /// `remove-value` / `remove-option` / `remove-unit` / `remove-barcode` / `remove-component`.
    Pick {
        /// What.
        kind: String,
        /// Row.
        row: usize,
        /// Sub-row.
        sub: usize,
    },
    /// The debounce of [`Effect::Debounce`] ended.
    SearchDue(u64),
    /// An event of login `generation`.
    Event(u64, ServerEvent),
    /// [`Effect::ReadFile`] finished.
    FileRead(Result<Vec<u8>, String>),
    /// Result of [`Effect::Login`].
    LoggedIn(Result<Login, ClientError>),
    /// Result of [`Effect::LoadMe`].
    Me(Result<Me, ClientError>),
    /// Result of [`Effect::LoadTaxClasses`].
    TaxClasses(Result<Vec<TaxClass>, ClientError>),
    /// Result of [`Effect::LoadCategories`].
    Categories(Result<Vec<CategoryNode>, ClientError>),
    /// Result of [`Effect::LoadBrands`].
    Brands(Result<Vec<Brand>, ClientError>),
    /// Result of [`Effect::LoadBranches`].
    Branches(Result<Vec<Branch>, ClientError>),
    /// Result of [`Effect::LoadSettings`].
    Settings(Result<CatalogSettings, ClientError>),
    /// Result of [`Effect::LoadProducts`].
    Products(bool, Result<ProductPage, ClientError>),
    /// Result of [`Effect::LoadProduct`].
    Product(Result<Box<Product>, ClientError>),
    /// Result of [`Effect::LoadBlocks`] (product ID, blocks).
    Blocks(String, Result<Vec<String>, ClientError>),
    /// Result of [`Effect::LoadImage`] (product ID, bytes).
    Image(String, Result<Option<Vec<u8>>, ClientError>),
    /// Result of [`Effect::SaveProduct`].
    Saved(Result<Box<Product>, ClientError>),
    /// Result of [`Effect::PutBlocks`].
    BlocksSaved(Result<Vec<String>, ClientError>),
    /// Result of [`Effect::PutImage`].
    ImageSaved(Result<ImageInfo, ClientError>),
    /// Result of [`Effect::DeleteImage`].
    ImageDeleted(Result<(), ClientError>),
    /// Result of [`Effect::SetArchived`].
    Archived(Result<Box<Product>, ClientError>),
    /// Result of [`Effect::Generate`].
    Generated(Result<String, ClientError>),
    /// Result of [`Effect::Search`].
    Hits(Result<Vec<SearchHit>, ClientError>),
    /// Result of [`Effect::LoadComponent`].
    Component(Result<Box<Product>, ClientError>),
    /// Result of [`Effect::CheckProduct`].
    Checked(Result<Box<Product>, ClientError>),
    /// Result of [`Effect::PutCategory`].
    CategorySaved(Result<Category, ClientError>),
    /// Result of [`Effect::PutBrand`].
    BrandSaved(Result<Brand, ClientError>),
    /// Result of [`Effect::Delete`].
    Deleted(Deletable, Result<(), ClientError>),
    /// Result of [`Effect::Logout`].
    LoggedOut(Result<(), ClientError>),
}

/// What [`Controller::update`] did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Step {
    /// Calls to make.
    pub effects: Vec<Effect>,
    /// For keys: whether the key was used.
    pub handled: bool,
}

impl Step {
    const fn handled(effects: Vec<Effect>) -> Self {
        Self {
            effects,
            handled: true,
        }
    }

    const fn effects(effects: Vec<Effect>) -> Self {
        Self {
            effects,
            handled: false,
        }
    }
}

fn notice(code: &'static str, arg: impl Into<String>, tone: Tone) -> Notice {
    Notice {
        code,
        arg: arg.into(),
        tone,
        until: None,
    }
}

/// The message for a failed call (codes of `crates/server/src/error.rs`).
#[must_use]
pub fn notice_for(error: &ClientError) -> Notice {
    let Some(p) = error.problem() else {
        return match error {
            ClientError::Network(_) => Notice::bad("network"),
            _ => notice("error", "unexpected", Tone::Bad),
        };
    };
    match p.code.as_str() {
        "invalid_credentials" => Notice::bad("wrong_password"),
        "locked_out" => Notice::bad("locked_out_bo"),
        "invalid_session" => Notice::bad("session_ended"),
        "permission_denied" => notice(
            "permission_denied_catalog",
            p.permission().unwrap_or(CATALOG_MANAGE).to_owned(),
            Tone::Bad,
        ),
        "read_only_replica" => Notice::bad("read_only_replica"),
        "invalid_image" => Notice::bad("invalid_image"),
        "payload_too_large" => Notice::bad("image_too_large"),
        "conflict" => Notice::bad("conflict_retry"),
        "not_found" => Notice::bad("not_found"),
        "duplicate_category_name" | "duplicate_brand_name" => Notice::bad("duplicate_name"),
        "category_cycle" => Notice::bad("category_cycle"),
        "category_too_deep" => Notice::bad("category_too_deep"),
        "in_use" => notice(
            "in_use",
            p.reason().unwrap_or_default().to_owned(),
            Tone::Bad,
        ),
        "invalid_field" | "invalid_code" => notice(
            "invalid_input",
            p.field().unwrap_or_default().to_owned(),
            Tone::Bad,
        ),
        other => notice("error", other.to_owned(), Tone::Bad),
    }
}

fn ends_login(error: &ClientError) -> bool {
    error.code() == Some("invalid_session")
}

fn key() -> String {
    new_id()
}

/// The back-office state; see the module docs.
#[derive(Debug, Default)]
pub struct Controller {
    /// The current screen.
    pub screen: Option<Screen>,
    /// The login screen.
    pub login: LoginState,
    /// The logged-in user.
    pub session: Option<Session>,
    /// Reference data.
    pub reference: Reference,
    /// The product list.
    pub list: ListState,
    /// The open form.
    pub form: Option<FormState>,
    /// The confirmation dialog.
    pub confirm: Option<Confirm>,
    /// The ID and name of what [`Confirm::Delete`] deletes.
    pub deleting: Option<(String, String)>,
    /// The category editor.
    pub categories: CategoryEditor,
    /// The brand list.
    pub brands: BrandEditor,
    /// A login is running.
    pub busy: bool,
    /// A field the view should focus (`search`, `name`, `category-name`, `brand-name`, or
    /// `list`: the window itself, for the shortcuts), with [`Controller::focus_serial`].
    pub focus: Option<&'static str>,
    /// Bumped per focus request.
    pub focus_serial: u64,
    /// Bumped when the form's texts change under the user (load, save, generated code): the
    /// view rebuilds the form so its fields show the new values.
    pub form_serial: u64,
    generation: u64,
    /// The window was opened from the Start screen: the login screen offers F9 to go back.
    pub start: bool,
}

impl Controller {
    /// The start state (login).
    #[must_use]
    pub fn new(username: &str) -> Self {
        let mut controller = Self {
            screen: Some(Screen::Login),
            login: LoginState {
                username: username.to_owned(),
                ..LoginState::default()
            },
            ..Self::default()
        };
        controller.focus(if username.is_empty() {
            "username"
        } else {
            "password"
        });
        controller
    }

    /// The current screen.
    #[must_use]
    pub fn screen(&self) -> Screen {
        self.screen.unwrap_or(Screen::Login)
    }

    fn token(&self) -> String {
        self.session
            .as_ref()
            .map(|s| s.token.clone())
            .unwrap_or_default()
    }

    /// Whether catalog writes are allowed (`catalog.manage` from any role assignment).
    #[must_use]
    pub fn can_manage(&self) -> bool {
        self.session.as_ref().is_some_and(Session::can_manage)
    }

    /// Whether archived entries may be listed.
    #[must_use]
    pub fn can_view_archived(&self) -> bool {
        self.session
            .as_ref()
            .is_some_and(Session::can_view_archived)
    }

    fn focus(&mut self, field: &'static str) {
        self.focus = Some(field);
        self.focus_serial += 1;
    }

    /// Handles one message; see the module docs.
    pub fn update(&mut self, msg: Msg) -> Step {
        match msg {
            Msg::Capture(press) => self.on_capture(press),
            Msg::Key(press) => self.on_key(press),
            Msg::Action(id) => Step::handled(self.on_action(&id)),
            Msg::Edit {
                kind,
                row,
                sub,
                field,
                value,
            } => Step::effects(self.on_edit(&kind, row, sub, &field, value)),
            Msg::Toggle {
                kind,
                row,
                field,
                on,
            } => Step::effects(self.on_toggle(&kind, row, &field, on)),
            Msg::Choose {
                kind,
                row,
                field,
                index,
            } => Step::effects(self.on_choose(&kind, row, &field, index)),
            Msg::Pick { kind, row, sub } => Step::effects(self.on_pick(&kind, row, sub)),
            Msg::SearchDue(serial) => {
                if serial == self.list.search_serial && self.session.is_some() {
                    Step::effects(self.reload_list())
                } else {
                    Step::default()
                }
            }
            Msg::Event(generation, event) => Step::effects(self.on_event(generation, &event)),
            result => Step::effects(self.on_result(result)),
        }
    }

    /// Keys before the focused widget: shortcuts with Ctrl, Esc, function keys, and the list
    /// keys (↑/↓/Enter) on the list screens.
    fn on_capture(&mut self, p: Press) -> Step {
        if let Some(confirm) = self.confirm {
            return match p.key {
                PressKey::Enter | PressKey::Char('y') if !p.ctrl => {
                    Step::handled(self.confirmed(confirm))
                }
                PressKey::Escape | PressKey::Char('n') if !p.ctrl => {
                    self.confirm = None;
                    self.deleting = None;
                    Step::handled(Vec::new())
                }
                _ => Step::handled(Vec::new()),
            };
        }
        let screen = self.screen();
        if screen == Screen::Login {
            if p.f(9) && self.start {
                return Step::handled(self.on_action("start"));
            }
            return Step::default();
        }
        for (n, target) in [
            (2, Screen::Products),
            (3, Screen::Categories),
            (4, Screen::Brands),
        ] {
            if p.f(n) {
                return Step::handled(self.go(target));
            }
        }
        if p.f(12) {
            return Step::handled(self.on_action("logout"));
        }
        match screen {
            Screen::Products => self.products_capture(p),
            Screen::Form => self.form_capture(p),
            Screen::Categories | Screen::Brands => self.taxonomy_capture(p, screen),
            Screen::Login => Step::default(),
        }
    }

    fn products_capture(&mut self, p: Press) -> Step {
        if p.ctrl_char('f') {
            self.focus("search");
            return Step::handled(Vec::new());
        }
        if p.ctrl_char('n') {
            return Step::handled(self.on_action("new-product"));
        }
        if p.ctrl_shift_char('d') {
            return Step::handled(self.on_action("delete-product"));
        }
        if p.f(5) {
            return Step::handled(self.reload_list());
        }
        if p.plain(PressKey::Up) {
            self.list.cursor = self.list.cursor.saturating_sub(1);
            return Step::handled(Vec::new());
        }
        if p.plain(PressKey::Down) {
            let last = self.list.rows.len().saturating_sub(1);
            if self.list.cursor < last {
                self.list.cursor += 1;
                return Step::handled(Vec::new());
            }
            return Step::handled(self.load_more());
        }
        if p.plain(PressKey::Enter) {
            return Step::handled(self.open_row(self.list.cursor));
        }
        if p.plain(PressKey::Escape) {
            self.focus("list");
            return Step::handled(Vec::new());
        }
        Step::default()
    }

    fn form_capture(&mut self, p: Press) -> Step {
        if p.ctrl_char('s') {
            return Step::handled(self.on_action("save"));
        }
        if p.ctrl_shift_char('a') {
            return Step::handled(self.on_action("archive"));
        }
        for (c, tab) in [('1', 0), ('2', 1), ('3', 2)] {
            if p.ctrl_char(c) {
                return Step::handled(self.on_action(&format!("tab-{tab}")));
            }
        }
        if p.plain(PressKey::Escape) {
            return Step::handled(self.on_action("back"));
        }
        Step::default()
    }

    fn taxonomy_capture(&mut self, p: Press, screen: Screen) -> Step {
        let prefix = if screen == Screen::Categories {
            "cat"
        } else {
            "brand"
        };
        if p.ctrl_char('s') {
            return Step::handled(self.on_action(&format!("{prefix}-save")));
        }
        if p.ctrl_char('n') {
            let action = if screen == Screen::Categories {
                "cat-add-child"
            } else {
                "brand-add"
            };
            return Step::handled(self.on_action(action));
        }
        if p.ctrl_shift_char('a') {
            return Step::handled(self.on_action(&format!("{prefix}-archive")));
        }
        if p.ctrl_shift_char('d') {
            return Step::handled(self.on_action(&format!("{prefix}-delete")));
        }
        if p.f(5) {
            return Step::handled(self.load_taxonomy());
        }
        let count = if screen == Screen::Categories {
            self.reference.categories.len()
        } else {
            self.reference.brands.len()
        };
        let cursor = if screen == Screen::Categories {
            self.categories.cursor
        } else {
            self.brands.cursor
        };
        let moved = if p.plain(PressKey::Up) {
            Some(cursor.saturating_sub(1))
        } else if p.plain(PressKey::Down) {
            Some((cursor + 1).min(count.saturating_sub(1)))
        } else {
            None
        };
        if let Some(row) = moved {
            let kind = if screen == Screen::Categories {
                "category"
            } else {
                "brand"
            };
            self.on_pick(kind, row, 0);
            return Step::handled(Vec::new());
        }
        if p.plain(PressKey::Escape) {
            return Step::handled(self.go(Screen::Products));
        }
        Step::default()
    }

    /// Keys no widget took: `/` focuses the search on the product list.
    fn on_key(&mut self, p: Press) -> Step {
        if self.confirm.is_none()
            && self.screen() == Screen::Products
            && p.key == PressKey::Char('/')
            && !p.ctrl
        {
            self.focus("search");
            return Step::handled(Vec::new());
        }
        Step::default()
    }

    /// Goes to `target`, asking first when the form has unsaved changes.
    fn go(&mut self, target: Screen) -> Vec<Effect> {
        if self.screen() == Screen::Form
            && self.form.as_ref().is_some_and(|f| f.form.is_dirty())
            && target != Screen::Form
        {
            self.confirm = Some(Confirm::Discard(target));
            return Vec::new();
        }
        self.enter(target)
    }

    fn enter(&mut self, target: Screen) -> Vec<Effect> {
        let from = self.screen();
        self.screen = Some(target);
        if from == Screen::Form && target != Screen::Form {
            self.form = None;
        }
        match target {
            Screen::Products => {
                self.focus("list");
                if from == Screen::Form || self.list.rows.is_empty() {
                    self.reload_list()
                } else {
                    Vec::new()
                }
            }
            Screen::Categories => {
                self.select_category(self.categories.cursor);
                self.focus("category-name");
                Vec::new()
            }
            Screen::Brands => {
                self.select_brand(self.brands.cursor);
                self.focus("brand-name");
                Vec::new()
            }
            Screen::Login | Screen::Form => Vec::new(),
        }
    }

    fn confirmed(&mut self, confirm: Confirm) -> Vec<Effect> {
        self.confirm = None;
        match confirm {
            Confirm::Discard(target) => self.enter(target),
            Confirm::Archive => {
                let Some(f) = &self.form else {
                    return Vec::new();
                };
                vec![Effect::SetArchived {
                    token: self.token(),
                    id: f.form.id.clone(),
                    archived: !f.form.archived,
                    key: key(),
                }]
            }
            Confirm::Delete(what) => self
                .deleting
                .take()
                .map(|(id, _)| {
                    vec![Effect::Delete {
                        token: self.token(),
                        what,
                        id,
                    }]
                })
                .unwrap_or_default(),
        }
    }

    /// Asks before deleting `what` (the highlighted product, or the edited category / brand).
    fn ask_delete(&mut self, what: Deletable) -> Vec<Effect> {
        let target = match what {
            Deletable::Product => self
                .list
                .rows
                .get(self.list.cursor)
                .map(|p| (p.id.clone(), p.name.clone())),
            Deletable::Category => self.categories.editing.as_ref().and_then(|id| {
                self.reference
                    .categories
                    .iter()
                    .find(|c| c.category.id == *id)
                    .map(|c| (id.clone(), c.category.name.clone()))
            }),
            Deletable::Brand => self.brands.editing.as_ref().and_then(|id| {
                self.reference
                    .brands
                    .iter()
                    .find(|b| b.id == *id)
                    .map(|b| (id.clone(), b.name.clone()))
            }),
        };
        let Some(target) = target else {
            return Vec::new();
        };
        if !self.can_manage() {
            let denied = Some(notice(
                "permission_denied_catalog",
                CATALOG_MANAGE,
                Tone::Bad,
            ));
            match what {
                Deletable::Product => self.list.notice = denied,
                Deletable::Category => self.categories.notice = denied,
                Deletable::Brand => self.brands.notice = denied,
            }
            return Vec::new();
        }
        self.deleting = Some(target);
        self.confirm = Some(Confirm::Delete(what));
        Vec::new()
    }

    fn reload_list(&mut self) -> Vec<Effect> {
        self.list.loading = true;
        self.list.next = None;
        vec![Effect::LoadProducts {
            token: self.token(),
            query: self.list.query(None),
            append: false,
        }]
    }

    fn load_more(&mut self) -> Vec<Effect> {
        let Some(next) = self.list.next.clone().filter(|_| !self.list.loading) else {
            return Vec::new();
        };
        self.list.loading = true;
        vec![Effect::LoadProducts {
            token: self.token(),
            query: self.list.query(Some(next)),
            append: true,
        }]
    }

    fn open_row(&mut self, row: usize) -> Vec<Effect> {
        let Some(id) = self.list.rows.get(row).map(|r| r.id.clone()) else {
            return Vec::new();
        };
        self.list.cursor = row;
        vec![Effect::LoadProduct {
            token: self.token(),
            id,
        }]
    }

    fn load_taxonomy(&self) -> Vec<Effect> {
        let token = self.token();
        let include_archived = self.can_view_archived();
        vec![
            Effect::LoadCategories {
                token: token.clone(),
                include_archived,
            },
            Effect::LoadBrands {
                token,
                include_archived,
            },
        ]
    }

    #[allow(clippy::too_many_lines)] // one arm per button
    fn on_action(&mut self, id: &str) -> Vec<Effect> {
        if let Some(confirm) = self.confirm {
            return match id {
                "confirm-yes" => self.confirmed(confirm),
                "confirm-no" => {
                    self.confirm = None;
                    self.deleting = None;
                    Vec::new()
                }
                _ => Vec::new(),
            };
        }
        match id {
            "login" => self.start_login(),
            "start" if self.start && self.screen() == Screen::Login && !self.busy => {
                vec![Effect::Exit]
            }
            "logout" => {
                let token = self.token();
                self.logged_out(None);
                vec![Effect::Logout(token)]
            }
            "nav-products" | "back" => self.go(Screen::Products),
            "nav-categories" => self.go(Screen::Categories),
            "nav-brands" => self.go(Screen::Brands),
            "refresh" => match self.screen() {
                Screen::Categories | Screen::Brands => self.load_taxonomy(),
                _ => self.reload_list(),
            },
            "load-more" => self.load_more(),
            "open" => self.open_row(self.list.cursor),
            "new-product" => self.new_product(),
            "delete-product" => self.ask_delete(Deletable::Product),
            "cat-delete" => self.ask_delete(Deletable::Category),
            "brand-delete" => self.ask_delete(Deletable::Brand),
            "save" => self.save(),
            "archive" => {
                if self.form.as_ref().is_some_and(|f| !f.form.is_new) && self.can_manage() {
                    self.confirm = Some(Confirm::Archive);
                }
                Vec::new()
            }
            "reload" => match &self.form {
                Some(f) if !f.form.is_new => vec![Effect::LoadProduct {
                    token: self.token(),
                    id: f.form.id.clone(),
                }],
                _ => Vec::new(),
            },
            "generate-code" => self.generate(Generate::ProductCode),
            "generate-barcode" => self.generate(Generate::Barcode),
            "add-option" => self.edit_form(ProductForm::add_option),
            "add-unit" => self.edit_form(ProductForm::add_unit),
            "add-barcode" => self.add_barcode(None),
            "pick-image" => {
                let path = self
                    .form
                    .as_ref()
                    .map(|f| f.image_path.trim().to_owned())
                    .unwrap_or_default();
                if path.is_empty() {
                    Vec::new()
                } else {
                    vec![Effect::ReadFile(path)]
                }
            }
            "remove-image" => {
                if let Some(f) = &mut self.form {
                    f.form.data.image = if f.form.has_image {
                        ImageEdit::Remove
                    } else {
                        ImageEdit::Keep
                    };
                    f.image = None;
                    f.image_serial += 1;
                }
                Vec::new()
            }
            "cat-add-root" | "cat-add-child" => {
                let parent = (id == "cat-add-child")
                    .then(|| self.categories.editing.clone())
                    .flatten();
                self.categories = CategoryEditor {
                    cursor: self.categories.cursor,
                    parent,
                    ..CategoryEditor::default()
                };
                self.focus("category-name");
                Vec::new()
            }
            "cat-save" => self.save_category(None),
            "cat-archive" => {
                let archived = self
                    .categories
                    .editing
                    .as_ref()
                    .and_then(|id| {
                        self.reference
                            .categories
                            .iter()
                            .find(|c| c.category.id == *id)
                    })
                    .map(|c| !c.category.archived);
                archived.map_or_else(Vec::new, |a| self.save_category(Some(a)))
            }
            "brand-add" => {
                self.brands = BrandEditor {
                    cursor: self.brands.cursor,
                    ..BrandEditor::default()
                };
                self.focus("brand-name");
                Vec::new()
            }
            "brand-save" => self.save_brand(None),
            "brand-archive" => {
                let archived = self
                    .brands
                    .editing
                    .as_ref()
                    .and_then(|id| self.reference.brands.iter().find(|b| b.id == *id))
                    .map(|b| !b.archived);
                archived.map_or_else(Vec::new, |a| self.save_brand(Some(a)))
            }
            other => other
                .strip_prefix("tab-")
                .and_then(|n| n.parse::<usize>().ok())
                .and_then(|n| Tab::ALL.get(n).copied())
                .map_or_else(Vec::new, |tab| {
                    if let Some(f) = &mut self.form {
                        f.form.tab = tab;
                        // The focused field of the old tab is gone.
                        self.focus("list");
                    }
                    Vec::new()
                }),
        }
    }

    fn start_login(&mut self) -> Vec<Effect> {
        if self.busy || self.screen() != Screen::Login {
            return Vec::new();
        }
        let username = self.login.username.trim().to_owned();
        if username.is_empty() || self.login.password.is_empty() {
            self.login.notice = Some(Notice::bad("login_required"));
            return Vec::new();
        }
        self.busy = true;
        self.login.notice = None;
        vec![Effect::Login {
            username,
            password: std::mem::take(&mut self.login.password),
        }]
    }

    /// Back to the login screen (logout, or the session ended).
    fn logged_out(&mut self, notice: Option<Notice>) {
        let username = self.login.username.clone();
        let generation = self.generation;
        let start = self.start;
        *self = Self::new(&username);
        self.generation = generation + 1;
        self.start = start;
        self.login.notice = notice;
    }

    fn edit_form(&mut self, f: fn(&mut ProductForm)) -> Vec<Effect> {
        if let Some(state) = &mut self.form
            && self.session.as_ref().is_some_and(Session::can_manage)
        {
            f(&mut state.form);
        }
        Vec::new()
    }

    fn new_product(&mut self) -> Vec<Effect> {
        if !self.can_manage() {
            self.list.notice = self
                .session
                .as_ref()
                .and_then(Session::read_only_reason)
                .map(|code| notice(code, "", Tone::Bad));
            return Vec::new();
        }
        let tax = self
            .reference
            .tax_classes
            .iter()
            .find(|t| t.name == "Standard")
            .or_else(|| self.reference.tax_classes.first())
            .map(|t| t.id.clone())
            .unwrap_or_default();
        let mut form = ProductForm::new_product(&tax);
        form.data.category_id.clone_from(&self.list.category);
        form.data.brand_id.clone_from(&self.list.brand);
        form.mark_clean();
        self.form = Some(FormState::new(form));
        self.form_serial += 1;
        self.screen = Some(Screen::Form);
        self.focus("name");
        Vec::new()
    }

    fn generate(&mut self, what: Generate) -> Vec<Effect> {
        let can = self.can_manage();
        let token = self.token();
        let Some(f) = self.form.as_mut().filter(|f| can && f.generating.is_none()) else {
            return Vec::new();
        };
        f.generating = Some(what);
        vec![Effect::Generate {
            token,
            what,
            key: key(),
        }]
    }

    /// Adds the picker's barcode (typed, or `generated`) for the picked variant + unit.
    fn add_barcode(&mut self, generated: Option<String>) -> Vec<Effect> {
        let Some(f) = &mut self.form else {
            return Vec::new();
        };
        let combos = f.form.active_combos();
        let units = f.form.active_units();
        let (Some(combo), Some((unit, _))) = (
            combos.get(f.pick_variant.min(combos.len().saturating_sub(1))),
            units.get(f.pick_unit.min(units.len().saturating_sub(1))),
        ) else {
            return Vec::new();
        };
        let code = generated.unwrap_or_else(|| std::mem::take(&mut f.new_barcode));
        if code.trim().is_empty() {
            return Vec::new();
        }
        let variant = f.form.data.combos[*combo].variant_id.clone();
        let unit = unit.clone();
        f.form.add_barcode(&variant, &unit, &code);
        self.form_serial += 1;
        Vec::new()
    }

    fn save(&mut self) -> Vec<Effect> {
        let can = self.can_manage();
        let reason = self.session.as_ref().and_then(Session::read_only_reason);
        let token = self.token();
        let Some(f) = &mut self.form else {
            return Vec::new();
        };
        if f.saving {
            return Vec::new();
        }
        if !can {
            f.notice = reason.map(|code| notice(code, "", Tone::Bad));
            return Vec::new();
        }
        if !f.form.validate() {
            f.notice = Some(Notice::bad("fix_errors"));
            return Vec::new();
        }
        f.saving = true;
        f.notice = None;
        vec![Effect::SaveProduct {
            token,
            id: f.form.id.clone(),
            body: Box::new(f.form.to_body()),
        }]
    }

    fn save_category(&mut self, archived: Option<bool>) -> Vec<Effect> {
        if !self.can_manage() {
            self.categories.notice = Some(notice(
                "permission_denied_catalog",
                CATALOG_MANAGE,
                Tone::Bad,
            ));
            return Vec::new();
        }
        let editor = &self.categories;
        let stored = editor.editing.as_ref().and_then(|id| {
            self.reference
                .categories
                .iter()
                .find(|c| c.category.id == *id)
        });
        let body = CategoryBody {
            parent_id: editor.parent.clone(),
            name: editor.name.trim().to_owned(),
            code: editor.code.trim().to_owned(),
            archived: archived.unwrap_or_else(|| stored.is_some_and(|c| c.category.archived)),
        };
        if body.name.is_empty() {
            self.categories.notice = Some(Notice::bad("name_required"));
            return Vec::new();
        }
        let id = editor.editing.clone().unwrap_or_else(new_id);
        vec![Effect::PutCategory {
            token: self.token(),
            id,
            body,
        }]
    }

    fn save_brand(&mut self, archived: Option<bool>) -> Vec<Effect> {
        if !self.can_manage() {
            self.brands.notice = Some(notice(
                "permission_denied_catalog",
                CATALOG_MANAGE,
                Tone::Bad,
            ));
            return Vec::new();
        }
        let stored = self
            .brands
            .editing
            .as_ref()
            .and_then(|id| self.reference.brands.iter().find(|b| b.id == *id));
        let body = BrandBody {
            name: self.brands.name.trim().to_owned(),
            archived: archived.unwrap_or_else(|| stored.is_some_and(|b| b.archived)),
        };
        if body.name.is_empty() {
            self.brands.notice = Some(Notice::bad("name_required"));
            return Vec::new();
        }
        let id = self.brands.editing.clone().unwrap_or_else(new_id);
        vec![Effect::PutBrand {
            token: self.token(),
            id,
            body,
        }]
    }

    fn select_category(&mut self, row: usize) {
        let Some(c) = self
            .reference
            .categories
            .get(row)
            .map(|r| r.category.clone())
        else {
            return;
        };
        self.categories = CategoryEditor {
            cursor: row,
            editing: Some(c.id),
            name: c.name,
            code: c.code,
            parent: c.parent_id,
            notice: None,
        };
    }

    fn select_brand(&mut self, row: usize) {
        let Some(b) = self.reference.brands.get(row).cloned() else {
            return;
        };
        self.brands = BrandEditor {
            cursor: row,
            editing: Some(b.id),
            name: b.name,
            notice: None,
        };
    }

    /// Parent choices of the category editor: `None` (a root) first, then every category
    /// except the edited one and its descendants (they would make a cycle).
    #[must_use]
    pub fn parent_choices(&self) -> Vec<(Option<String>, String)> {
        let mut out = vec![(None, String::new())];
        let mut skip_depth: Option<usize> = None;
        for row in &self.reference.categories {
            if let Some(depth) = skip_depth {
                if row.depth > depth {
                    continue;
                }
                skip_depth = None;
            }
            if self.categories.editing.as_deref() == Some(row.category.id.as_str()) {
                skip_depth = Some(row.depth);
                continue;
            }
            out.push((
                Some(row.category.id.clone()),
                format!("{}{}", "  ".repeat(row.depth), row.category.name),
            ));
        }
        out
    }

    #[allow(clippy::too_many_lines)] // one arm per editable field
    fn on_edit(
        &mut self,
        kind: &str,
        row: usize,
        sub: usize,
        field: &str,
        value: String,
    ) -> Vec<Effect> {
        match kind {
            "login" => {
                match field {
                    "username" => self.login.username = value,
                    "password" => self.login.password = value,
                    _ => {}
                }
                return Vec::new();
            }
            "list" => {
                self.list.search = value;
                self.list.search_serial += 1;
                return vec![Effect::Debounce(self.list.search_serial)];
            }
            "category" => {
                match field {
                    "name" => self.categories.name = value,
                    "code" => self.categories.code = value,
                    _ => {}
                }
                return Vec::new();
            }
            "brand" => {
                self.brands.name = value;
                return Vec::new();
            }
            _ => {}
        }
        let can = self.can_manage();
        let token = self.token();
        let Some(f) = &mut self.form else {
            return Vec::new();
        };
        // Side fields that are not part of the document.
        match (kind, field) {
            ("form", "image_path") => {
                f.image_path = value;
                return Vec::new();
            }
            ("form", "new_barcode") => {
                f.new_barcode = value;
                return Vec::new();
            }
            ("form", "component_search") => {
                f.component_search.clone_from(&value);
                return if value.trim().is_empty() {
                    f.hits.clear();
                    Vec::new()
                } else {
                    vec![Effect::Search {
                        token,
                        q: value.trim().to_owned(),
                    }]
                };
            }
            _ => {}
        }
        if !can {
            return Vec::new();
        }
        let d = &mut f.form.data;
        match (kind, field) {
            ("form", "code") => d.code = value,
            ("form", "name") => d.name = value,
            ("form", "receipt_name") => d.receipt_name = value,
            ("form", "tags") => d.tags = value,
            ("option", "name") => {
                if let Some(o) = d.options.get_mut(row) {
                    o.name = value;
                }
            }
            ("value", "name" | "code") => {
                if let Some(v) = d.options.get_mut(row).and_then(|o| o.values.get_mut(sub)) {
                    if field == "name" {
                        v.name = value;
                    } else {
                        v.code = value;
                    }
                }
            }
            ("combo", "sku") => {
                if let Some(c) = d.combos.get_mut(row) {
                    c.sku = value;
                }
            }
            ("combo", "plu") => {
                if let Some(c) = d.combos.get_mut(row) {
                    c.plu = value;
                }
            }
            ("unit", "name" | "short_name" | "factor") => {
                let locked = f.form.factor_locked(row);
                if let Some(u) = f.form.data.units.get_mut(row) {
                    match field {
                        "name" => u.name = value,
                        "short_name" => u.short_name = value,
                        _ if !locked => u.factor = value,
                        _ => {}
                    }
                }
            }
            ("barcode", "code") => {
                if let Some(b) = d.barcodes.get_mut(row) {
                    b.code = value;
                }
            }
            ("component", "qty") => {
                if let Some(c) = d.components.get_mut(row) {
                    c.qty = value;
                }
            }
            _ => {}
        }
        Vec::new()
    }

    fn on_toggle(&mut self, kind: &str, row: usize, field: &str, on: bool) -> Vec<Effect> {
        if (kind, field) == ("list", "archived") {
            if !self.can_view_archived() {
                return Vec::new();
            }
            self.list.archived = on;
            return self.reload_list();
        }
        let can = self.can_manage();
        let branches: Vec<String> = self
            .reference
            .branches
            .iter()
            .map(|b| b.id.clone())
            .collect();
        let Some(f) = self.form.as_mut().filter(|_| can) else {
            return Vec::new();
        };
        let d = &mut f.form.data;
        match (kind, field) {
            ("form", "open_price") => d.open_price = on,
            ("form", "requires_note") => d.requires_note = on,
            ("form", "sellable") => d.sellable = on,
            ("form", "purchasable") => d.purchasable = on,
            ("branch", "sold") => {
                if let Some(id) = branches.get(row) {
                    if on {
                        d.blocked.remove(id);
                    } else {
                        d.blocked.insert(id.clone());
                    }
                }
            }
            ("combo", "on") => f.form.set_combo_on(row, on),
            ("unit", "sellable") => {
                if let Some(u) = d.units.get_mut(row) {
                    u.sellable = on;
                }
            }
            ("unit", "purchasable") => {
                if let Some(u) = d.units.get_mut(row) {
                    u.purchasable = on;
                }
            }
            _ => {}
        }
        Vec::new()
    }

    #[allow(clippy::too_many_lines)] // one arm per picker
    fn on_choose(&mut self, kind: &str, row: usize, field: &str, index: usize) -> Vec<Effect> {
        match (kind, field) {
            ("list", "category") => {
                self.list.category = self
                    .reference
                    .category_choices(None)
                    .get(index)
                    .and_then(|c| c.0.clone());
                return self.reload_list();
            }
            ("list", "brand") => {
                self.list.brand = self
                    .reference
                    .brand_choices(None)
                    .get(index)
                    .and_then(|c| c.0.clone());
                return self.reload_list();
            }
            ("list", "type") => {
                self.list.product_type = index.checked_sub(1).and_then(|i| TYPES.get(i).copied());
                return self.reload_list();
            }
            ("category", "parent") => {
                self.categories.parent = self.parent_choices().get(index).and_then(|c| c.0.clone());
                return Vec::new();
            }
            _ => {}
        }
        let can = self.can_manage();
        let categories = self
            .form
            .as_ref()
            .map(|f| {
                self.reference
                    .category_choices(f.form.data.category_id.as_deref())
            })
            .unwrap_or_default();
        let brands = self
            .form
            .as_ref()
            .map(|f| {
                self.reference
                    .brand_choices(f.form.data.brand_id.as_deref())
            })
            .unwrap_or_default();
        let taxes: Vec<String> = self
            .reference
            .tax_classes
            .iter()
            .map(|t| t.id.clone())
            .collect();
        let Some(f) = &mut self.form else {
            return Vec::new();
        };
        match (kind, field) {
            ("form", "pick_variant") => f.pick_variant = index,
            ("form", "pick_unit") => f.pick_unit = index,
            _ if !can => {}
            ("form", "type") => {
                if let Some(t) = TYPES.get(index) {
                    f.form.set_type(*t);
                }
            }
            ("form", "category") => {
                if let Some(c) = categories.get(index) {
                    f.form.data.category_id.clone_from(&c.0);
                }
            }
            ("form", "brand") => {
                if let Some(b) = brands.get(index) {
                    f.form.data.brand_id.clone_from(&b.0);
                }
            }
            ("form", "tax_class") => {
                if let Some(t) = taxes.get(index) {
                    f.form.data.tax_class_id.clone_from(t);
                }
            }
            ("form", "base_kind")
                if !f.form.used
                    && matches!(
                        f.form.data.product_type,
                        ProductType::Stock | ProductType::Service
                    ) =>
            {
                f.form.data.base_kind = if index == 1 {
                    sultan_core::catalog::UnitKind::Decimal
                } else {
                    sultan_core::catalog::UnitKind::Integer
                };
            }
            ("form", "default_sale" | "default_purchase") => {
                if let Some((id, _)) = f.form.active_units().get(index) {
                    if field == "default_sale" {
                        f.form.data.default_sale.clone_from(id);
                    } else {
                        f.form.data.default_purchase.clone_from(id);
                    }
                }
            }
            ("component", "unit") => {
                if let Some(c) = f.form.data.components.get_mut(row)
                    && let Some((id, _)) = c.units.get(index)
                {
                    c.unit_id.clone_from(id);
                }
            }
            _ => {}
        }
        Vec::new()
    }

    fn on_pick(&mut self, kind: &str, row: usize, sub: usize) -> Vec<Effect> {
        match kind {
            "products" => {
                if row < self.list.rows.len() {
                    self.list.cursor = row;
                }
                return Vec::new();
            }
            "open" => return self.open_row(row),
            "category" => {
                self.select_category(row);
                return Vec::new();
            }
            "brand" => {
                self.select_brand(row);
                return Vec::new();
            }
            _ => {}
        }
        let can = self.can_manage();
        let token = self.token();
        let Some(f) = self.form.as_mut().filter(|_| can) else {
            return Vec::new();
        };
        match kind {
            "remove-option" => f.form.remove_option(row),
            "add-value" => f.form.add_value(row),
            "remove-value" => f.form.remove_value(row, sub),
            "remove-unit" => f.form.remove_unit(row),
            "remove-barcode" if row < f.form.data.barcodes.len() => {
                f.form.data.barcodes.remove(row);
            }
            "remove-component" if row < f.form.data.components.len() => {
                f.form.data.components.remove(row);
            }
            "hit" => {
                let Some(hit) = f.hits.get(row).cloned() else {
                    return Vec::new();
                };
                f.form.add_component(ComponentRow {
                    variant_id: hit.variant_id,
                    unit_id: hit.unit_id,
                    qty: "1".to_owned(),
                    product_id: Some(hit.product_id.clone()),
                    name: hit.name,
                    sku: hit.sku,
                    units: Vec::new(),
                });
                f.hits.clear();
                f.component_search.clear();
                self.form_serial += 1;
                return vec![Effect::LoadComponent {
                    token,
                    id: hit.product_id,
                }];
            }
            _ => {}
        }
        Vec::new()
    }

    fn on_event(&mut self, generation: u64, event: &ServerEvent) -> Vec<Effect> {
        if generation != self.generation || self.session.is_none() {
            return Vec::new();
        }
        if event.revokes_this_session() {
            self.logged_out(Some(Notice::bad("session_ended")));
            return Vec::new();
        }
        if event.kind != "catalog.changed" {
            return Vec::new();
        }
        match event.data["scope"].as_str() {
            Some("product") => {
                let id = event.data["product_id"].as_str().unwrap_or_default();
                match self.screen() {
                    Screen::Products => self.reload_list(),
                    Screen::Form => match &self.form {
                        Some(f) if f.form.id == id && !f.saving && !f.form.is_new => {
                            vec![Effect::CheckProduct {
                                token: self.token(),
                                id: id.to_owned(),
                            }]
                        }
                        _ => Vec::new(),
                    },
                    _ => Vec::new(),
                }
            }
            Some("category" | "brand") => self.load_taxonomy(),
            _ => Vec::new(),
        }
    }

    /// A failed call: the login ended (back to login), or `notice` gets the message.
    fn failed(&mut self, error: &ClientError) -> Option<Notice> {
        if ends_login(error) {
            self.logged_out(Some(Notice::bad("session_ended")));
            return None;
        }
        Some(notice_for(error))
    }

    #[allow(clippy::too_many_lines)] // one arm per result
    fn on_result(&mut self, msg: Msg) -> Vec<Effect> {
        match msg {
            Msg::LoggedIn(result) => {
                self.busy = false;
                match result {
                    Ok(login) => {
                        let token = login.token.clone();
                        self.generation += 1;
                        self.session = Some(Session {
                            token: token.clone(),
                            me: Me::default(),
                            display_name: login.display_name,
                        });
                        self.screen = Some(Screen::Products);
                        self.focus("list");
                        let mut effects = vec![
                            Effect::LoadMe(token.clone()),
                            Effect::LoadTaxClasses(token.clone()),
                            Effect::LoadBranches(token.clone()),
                            Effect::LoadSettings(token.clone()),
                            Effect::WatchEvents {
                                token,
                                generation: self.generation,
                            },
                        ];
                        effects.extend(self.reload_list());
                        effects
                    }
                    Err(e) => {
                        self.login.notice = Some(notice_for(&e));
                        self.focus("password");
                        Vec::new()
                    }
                }
            }
            Msg::Me(result) => match result {
                Ok(me) => {
                    if let Some(s) = &mut self.session {
                        s.me = me;
                    }
                    self.load_taxonomy()
                }
                Err(e) => {
                    self.list.notice = self.failed(&e);
                    Vec::new()
                }
            },
            Msg::TaxClasses(Ok(classes)) => {
                self.reference.tax_classes = classes;
                Vec::new()
            }
            Msg::Categories(Ok(tree)) => {
                let mut rows = Vec::new();
                flatten(&tree, 0, &mut rows);
                self.reference.categories = rows;
                if self.screen() == Screen::Categories {
                    let editing = self.categories.editing.clone();
                    let row = editing.and_then(|id| {
                        self.reference
                            .categories
                            .iter()
                            .position(|c| c.category.id == id)
                    });
                    if let Some(row) = row {
                        let notice = self.categories.notice.take();
                        self.select_category(row);
                        self.categories.notice = notice;
                    }
                }
                Vec::new()
            }
            Msg::Brands(Ok(brands)) => {
                self.reference.brands = brands;
                if self.screen() == Screen::Brands {
                    let editing = self.brands.editing.clone();
                    let row = editing
                        .and_then(|id| self.reference.brands.iter().position(|b| b.id == id));
                    if let Some(row) = row {
                        let notice = self.brands.notice.take();
                        self.select_brand(row);
                        self.brands.notice = notice;
                    }
                }
                Vec::new()
            }
            Msg::Branches(Ok(branches)) => {
                self.reference.branches = branches;
                Vec::new()
            }
            Msg::Settings(Ok(settings)) => {
                self.reference.settings = settings;
                Vec::new()
            }
            Msg::TaxClasses(Err(e))
            | Msg::Categories(Err(e))
            | Msg::Brands(Err(e))
            | Msg::Branches(Err(e))
            | Msg::Settings(Err(e)) => {
                self.list.notice = self.failed(&e);
                Vec::new()
            }
            Msg::Products(append, result) => {
                self.list.loading = false;
                match result {
                    Ok(page) => {
                        if append {
                            self.list.rows.extend(page.items);
                        } else {
                            self.list.rows = page.items;
                            self.list.cursor =
                                self.list.cursor.min(self.list.rows.len().saturating_sub(1));
                        }
                        self.list.next = page.next_before_id;
                        self.list.notice = self.list.notice.take().filter(|n| n.tone == Tone::Good);
                    }
                    Err(e) => self.list.notice = self.failed(&e),
                }
                Vec::new()
            }
            Msg::Product(result) => {
                match result {
                    Ok(p) => {
                        let form = ProductForm::from_product(&p, &[], false);
                        self.form = Some(FormState::new(form));
                        self.form_serial += 1;
                        self.screen = Some(Screen::Form);
                        self.focus("name");
                        // Blocks, image and component units once the form exists.
                        let token = self.token();
                        let mut effects = vec![
                            Effect::LoadBlocks {
                                token: token.clone(),
                                id: p.id.clone(),
                            },
                            Effect::LoadImage {
                                token: token.clone(),
                                id: p.id.clone(),
                            },
                        ];
                        let mut seen = BTreeSet::new();
                        effects.extend(
                            p.components
                                .iter()
                                .filter_map(|c| c.product_id.clone())
                                .filter(|id| seen.insert(id.clone()))
                                .map(|id| Effect::LoadComponent {
                                    token: token.clone(),
                                    id,
                                }),
                        );
                        return effects;
                    }
                    Err(e) => self.list.notice = self.failed(&e),
                }
                Vec::new()
            }
            Msg::Blocks(id, result) => {
                if let (Some(f), Ok(blocks)) = (&mut self.form, result)
                    && f.form.id == id
                {
                    let set: BTreeSet<String> = blocks.into_iter().collect();
                    let dirty = f.form.is_dirty();
                    f.form.data.blocked.clone_from(&set);
                    if !dirty {
                        f.form.mark_clean();
                    }
                    f.stored_blocks = set;
                }
                Vec::new()
            }
            Msg::Image(id, result) => {
                if let (Some(f), Ok(bytes)) = (&mut self.form, result)
                    && f.form.id == id
                {
                    f.form.has_image = bytes.is_some();
                    f.image = bytes;
                    f.image_serial += 1;
                }
                Vec::new()
            }
            Msg::FileRead(result) => {
                if let Some(f) = &mut self.form {
                    match result {
                        Ok(bytes) => {
                            f.form.data.image = ImageEdit::Replace(bytes.clone());
                            f.image = Some(bytes);
                            f.image_serial += 1;
                            f.notice = None;
                        }
                        Err(reason) => f.notice = Some(notice("file_error", reason, Tone::Bad)),
                    }
                }
                Vec::new()
            }
            Msg::Saved(result) => self.saved(result),
            Msg::BlocksSaved(result) => {
                let failure = result.as_ref().err().cloned();
                if let (Some(f), Ok(blocks)) = (&mut self.form, result) {
                    f.stored_blocks = blocks.into_iter().collect();
                }
                self.save_step_done(failure.as_ref());
                Vec::new()
            }
            Msg::ImageSaved(result) => {
                let failure = result.err();
                if failure.is_none()
                    && let Some(f) = &mut self.form
                {
                    f.form.has_image = true;
                }
                self.save_step_done(failure.as_ref());
                Vec::new()
            }
            Msg::ImageDeleted(result) => {
                let failure = result.err();
                if failure.is_none()
                    && let Some(f) = &mut self.form
                {
                    f.form.has_image = false;
                }
                self.save_step_done(failure.as_ref());
                Vec::new()
            }
            Msg::Archived(result) => {
                match result {
                    Ok(p) => {
                        if let Some(f) = &mut self.form {
                            let dirty = f.form.is_dirty();
                            f.form.archived = p.archived;
                            f.form.updated_at = p.updated_at;
                            if !dirty {
                                f.form.mark_clean();
                            }
                            f.notice = Some(notice(
                                if p.archived {
                                    "archived_done"
                                } else {
                                    "unarchived_done"
                                },
                                "",
                                Tone::Good,
                            ));
                        }
                    }
                    Err(e) => {
                        let n = self.failed(&e);
                        if let Some(f) = &mut self.form {
                            if let Some(p) = e.problem() {
                                f.form.apply_problem(p);
                            }
                            f.notice = n;
                        }
                    }
                }
                Vec::new()
            }
            Msg::Generated(result) => {
                let what = self.form.as_mut().and_then(|f| f.generating.take());
                match result {
                    Ok(value) => match what {
                        Some(Generate::ProductCode) => {
                            if let Some(f) = &mut self.form {
                                f.form.data.code = value;
                                self.form_serial += 1;
                                self.focus("list");
                            }
                        }
                        Some(Generate::Barcode) => return self.add_barcode(Some(value)),
                        None => {}
                    },
                    Err(e) => {
                        let n = self.failed(&e);
                        if let Some(f) = &mut self.form {
                            f.notice = n;
                        }
                    }
                }
                Vec::new()
            }
            Msg::Hits(result) => {
                if let (Some(f), Ok(hits)) = (&mut self.form, result) {
                    let own = f.form.id.clone();
                    f.hits = hits
                        .into_iter()
                        .filter(|h| h.product_id != own && h.product_type != "bundle")
                        .collect();
                }
                Vec::new()
            }
            Msg::Component(result) => {
                if let (Some(f), Ok(p)) = (&mut self.form, result) {
                    let units: Vec<(String, String)> = p
                        .units
                        .iter()
                        .filter(|u| !u.archived)
                        .map(|u| (u.id.clone(), u.name.clone()))
                        .collect();
                    let dirty = f.form.is_dirty();
                    f.form.set_component_units(&p.id, &units);
                    if !dirty {
                        f.form.mark_clean();
                    }
                }
                Vec::new()
            }
            Msg::Checked(result) => {
                if let (Some(f), Ok(p)) = (&mut self.form, result)
                    && f.form.id == p.id
                    && !f.saving
                    && p.updated_at > f.form.updated_at
                {
                    f.changed_elsewhere = true;
                }
                Vec::new()
            }
            Msg::CategorySaved(result) => {
                match result {
                    Ok(c) => {
                        self.categories.editing = Some(c.id);
                        self.categories.notice = Some(notice("saved", "", Tone::Good));
                        return self.load_taxonomy();
                    }
                    Err(e) => self.categories.notice = self.failed(&e),
                }
                Vec::new()
            }
            Msg::BrandSaved(result) => {
                match result {
                    Ok(b) => {
                        self.brands.editing = Some(b.id);
                        self.brands.notice = Some(notice("saved", "", Tone::Good));
                        return self.load_taxonomy();
                    }
                    Err(e) => self.brands.notice = self.failed(&e),
                }
                Vec::new()
            }
            Msg::Deleted(what, result) => self.deleted(what, &result),
            _ => Vec::new(),
        }
    }

    /// A delete answered: on success the list (or the taxonomy) reloads and the editor starts a
    /// new entry; `in_use` explains to archive instead.
    fn deleted(&mut self, what: Deletable, result: &Result<(), ClientError>) -> Vec<Effect> {
        let outcome = match result {
            Ok(()) => Some(notice("deleted_done", "", Tone::Good)),
            Err(e) => self.failed(e),
        };
        if self.session.is_none() {
            return Vec::new();
        }
        match what {
            Deletable::Product => {
                self.list.notice = outcome;
                if result.is_ok() {
                    self.list.cursor = self.list.cursor.saturating_sub(1);
                    return self.reload_list();
                }
            }
            Deletable::Category => {
                if result.is_ok() {
                    self.categories = CategoryEditor {
                        cursor: self.categories.cursor.saturating_sub(1),
                        ..CategoryEditor::default()
                    };
                    self.categories.notice = outcome;
                    return self.load_taxonomy();
                }
                self.categories.notice = outcome;
            }
            Deletable::Brand => {
                if result.is_ok() {
                    self.brands = BrandEditor {
                        cursor: self.brands.cursor.saturating_sub(1),
                        ..BrandEditor::default()
                    };
                    self.brands.notice = outcome;
                    return self.load_taxonomy();
                }
                self.brands.notice = outcome;
            }
        }
        Vec::new()
    }

    /// The product `PUT` answered: on success the branch blocks and the image follow.
    fn saved(&mut self, result: Result<Box<Product>, ClientError>) -> Vec<Effect> {
        let token = self.token();
        let failure_notice = result.as_ref().err().and_then(|e| self.failed(e));
        let Some(f) = &mut self.form else {
            return Vec::new();
        };
        match result {
            Ok(p) => {
                let blocked = f.form.data.blocked.clone();
                let image = f.form.data.image.clone();
                let tab = f.form.tab;
                let mut form = ProductForm::from_product(
                    &p,
                    &blocked.iter().cloned().collect::<Vec<_>>(),
                    f.form.has_image,
                );
                form.tab = tab;
                // Component unit choices survive the reload.
                for (new, old) in form.data.components.iter_mut().zip(&f.form.data.components) {
                    if new.units.is_empty() {
                        new.units.clone_from(&old.units);
                    }
                }
                form.mark_clean();
                f.form = form;
                self.form_serial += 1;
                self.focus = Some("list");
                self.focus_serial += 1;
                let mut effects = Vec::new();
                if blocked != f.stored_blocks {
                    effects.push(Effect::PutBlocks {
                        token: token.clone(),
                        id: p.id.clone(),
                        branch_ids: blocked.into_iter().collect(),
                    });
                }
                match image {
                    ImageEdit::Replace(bytes) => effects.push(Effect::PutImage {
                        token,
                        id: p.id.clone(),
                        bytes,
                    }),
                    ImageEdit::Remove => effects.push(Effect::DeleteImage {
                        token,
                        id: p.id.clone(),
                    }),
                    ImageEdit::Keep => {}
                }
                f.outstanding = effects.len();
                f.changed_elsewhere = false;
                if effects.is_empty() {
                    f.saving = false;
                    f.notice = Some(notice("saved", "", Tone::Good));
                }
                effects
            }
            Err(e) => {
                f.saving = false;
                if e.problem().is_some_and(|p| f.form.apply_problem(p)) {
                    f.notice = Some(Notice::bad("fix_errors"));
                } else {
                    f.notice = failure_notice;
                }
                Vec::new()
            }
        }
    }

    /// One of the calls after the product `PUT` finished (the branch blocks or the image).
    fn save_step_done(&mut self, failure: Option<&ClientError>) {
        let failed = failure.and_then(|e| self.failed(e));
        let Some(f) = &mut self.form else {
            return;
        };
        f.outstanding = f.outstanding.saturating_sub(1);
        if failed.is_some() {
            f.notice = failed;
        }
        if f.outstanding == 0 {
            f.saving = false;
            if !f.notice.as_ref().is_some_and(|n| n.tone == Tone::Bad) {
                f.notice = Some(notice("saved", "", Tone::Good));
            }
        }
    }
}

#[cfg(test)]
mod tests;
