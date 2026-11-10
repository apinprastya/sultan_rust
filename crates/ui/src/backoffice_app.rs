//! Glue between the [`BackOfficeWindow`] and the back-office [`Controller`] (phase C4), the same
//! way [`crate::app`] drives the register window: callbacks become [`Msg`]s, [`Effect`]s become
//! [`BackOffice`] calls whose answers come back as messages, and the window is rendered from the
//! controller after each batch. [`Runner::Block`] awaits every call on the spot (tests);
//! [`Runner::Spawn`] runs them on the runtime and wakes the window through `pump`.
//!
//! Besides the calls the glue does three small things the pure controller cannot:
//! - **debounce** ([`Effect::Debounce`]): a single-shot `slint::Timer` (immediately with a zero
//!   [`BackOfficeOptions::debounce`], as in tests);
//! - **files** ([`Effect::ReadFile`]): reads the image the user picked;
//! - **thumbnails**: decodes the product image (PNG/JPEG/WebP) into a `slint::Image`.
//!
//! Rows that hold text fields are updated row by row, so typing never rebuilds the field that
//! has the focus; when rows are added or removed the form is rebuilt (`form-serial`).

use std::cell::{Cell, Ref, RefCell};
use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};
use sultan_client::{BackOffice, EventSink};
use sultan_core::catalog::{ProductType, UnitKind};
use tokio::runtime::Handle;

use crate::app::{Runner, Watchers};
use crate::backoffice::form::{FormError, ProductForm, Tab};
use crate::backoffice::{Confirm, Controller, Effect, Msg, Press, Screen, TYPES};
use crate::controller::Notice;
use crate::{
    BackOfficeWindow, BoBarcode, BoBranch, BoBrandEditor, BoCategoryEditor, BoCombo, BoComponent,
    BoForm, BoGeneral, BoHit, BoList, BoLogin, BoOption, BoProductRow, BoTreeRow, BoUnit, BoValue,
    Note,
};

/// What the back-office window needs besides the service.
#[derive(Debug, Clone)]
pub struct BackOfficeOptions {
    /// The server, shown on the login screen and in the header.
    pub server: String,
    /// The username to start with.
    pub username: String,
    /// `id` or `en`; `None` keeps Slint's choice from the system locale.
    pub language: Option<String>,
    /// How long the search waits after the last keystroke (zero: at once).
    pub debounce: Duration,
    /// Opened from the Start screen: the login screen offers F9 to go back to it (the window
    /// closes).
    pub start: bool,
}

impl BackOfficeOptions {
    /// Options for `server` with a 300 ms search debounce and the system language.
    #[must_use]
    pub fn new(server: impl Into<String>) -> Self {
        Self {
            server: server.into(),
            username: String::new(),
            language: None,
            debounce: Duration::from_millis(300),
            start: false,
        }
    }
}

type Inbox = Arc<Mutex<VecDeque<Msg>>>;
type Wake = Arc<dyn Fn() + Send + Sync>;
type MsgFuture = Pin<Box<dyn Future<Output = Msg> + Send>>;

fn push(inbox: &Inbox, msg: Msg) {
    inbox
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .push_back(msg);
}

/// Replaces the rows of `model` if they changed (lists without text fields).
fn sync<T: Clone + PartialEq + 'static>(model: &VecModel<T>, rows: Vec<T>) {
    if model.iter().ne(rows.iter().cloned()) {
        model.set_vec(rows);
    }
}

/// Updates `model` row by row: changed rows are set, extra rows pushed or removed. A row whose
/// text field has the focus keeps its element.
fn sync_rows<T: Clone + PartialEq + 'static>(model: &VecModel<T>, rows: &[T]) {
    let count = model.row_count();
    for (i, row) in rows.iter().enumerate() {
        if i < count {
            if model.row_data(i).as_ref() != Some(row) {
                model.set_row_data(i, row.clone());
            }
        } else {
            model.push(row.clone());
        }
    }
    for i in (rows.len()..count).rev() {
        model.remove(i);
    }
}

#[derive(Default)]
struct Models {
    rows: Rc<VecModel<BoProductRow>>,
    filter_categories: Rc<VecModel<SharedString>>,
    filter_brands: Rc<VecModel<SharedString>>,
    options: Rc<VecModel<BoOption>>,
    values: RefCell<Vec<Rc<VecModel<BoValue>>>>,
    combos: Rc<VecModel<BoCombo>>,
    units: Rc<VecModel<BoUnit>>,
    barcodes: Rc<VecModel<BoBarcode>>,
    components: Rc<VecModel<BoComponent>>,
    hits: Rc<VecModel<BoHit>>,
    branches: Rc<VecModel<BoBranch>>,
    category_choices: Rc<VecModel<SharedString>>,
    brand_choices: Rc<VecModel<SharedString>>,
    tax_choices: Rc<VecModel<SharedString>>,
    unit_choices: Rc<VecModel<SharedString>>,
    variant_choices: Rc<VecModel<SharedString>>,
    tab_errors: Rc<VecModel<bool>>,
    category_rows: Rc<VecModel<BoTreeRow>>,
    parent_choices: Rc<VecModel<SharedString>>,
    brand_rows: Rc<VecModel<BoTreeRow>>,
}

struct State {
    watchers: Watchers,
    ui: slint::Weak<BackOfficeWindow>,
    controller: RefCell<Controller>,
    inbox: Inbox,
    wake: Wake,
    service: Arc<dyn BackOffice>,
    runner: Runner,
    options: BackOfficeOptions,
    models: Models,
    draining: Cell<bool>,
    /// Row counts of the form at the last render (a change rebuilds the form).
    shape: RefCell<Vec<usize>>,
    /// Bumped when the shape changes.
    rebuilds: Cell<u64>,
    /// The controller's focus serial at the last form render.
    focus_seen: Cell<u64>,
    /// Focus requests of the glue itself (after a rebuild).
    focus_bumps: Cell<u64>,
    /// The decoded thumbnail and the (product, serial) it is for.
    thumbnail: RefCell<(String, u64, slint::Image)>,
    debounce: RefCell<Option<slint::Timer>>,
}

/// The back-office window and its state.
pub struct BackOfficeApp {
    /// The window.
    pub ui: BackOfficeWindow,
    state: Rc<State>,
}

impl BackOfficeApp {
    /// The controller (read-only), for tests.
    #[must_use]
    pub fn controller(&self) -> Ref<'_, Controller> {
        self.state.controller.borrow()
    }

    /// Sends a message as if the window had; returns whether a key was used.
    #[must_use = "whether a key was used"]
    pub fn dispatch(&self, msg: Msg) -> bool {
        self.state.dispatch(msg)
    }

    /// Handles queued results and events (what `pump` does on the event loop).
    pub fn pump(&self) {
        self.state.drain();
        self.state.render();
    }
}

/// Starts the call of `effect`; `None` for the effects the glue handles itself.
#[allow(clippy::too_many_lines)] // one arm per call
fn start(service: &dyn BackOffice, effect: &Effect) -> Option<MsgFuture> {
    macro_rules! call {
        ($future:expr, $msg:expr) => {{
            let future = $future;
            let wrap = $msg;
            Some(Box::pin(async move { wrap(future.await) }) as MsgFuture)
        }};
    }
    match effect {
        Effect::Login { username, password } => {
            call!(service.password_login(username, password), Msg::LoggedIn)
        }
        Effect::LoadMe(token) => call!(service.me(token), Msg::Me),
        Effect::LoadTaxClasses(token) => call!(service.tax_classes(token), Msg::TaxClasses),
        Effect::LoadCategories {
            token,
            include_archived,
        } => call!(
            service.categories(token, *include_archived),
            Msg::Categories
        ),
        Effect::LoadBrands {
            token,
            include_archived,
        } => call!(service.brands(token, *include_archived), Msg::Brands),
        Effect::LoadBranches(token) => call!(service.branches(token), Msg::Branches),
        Effect::LoadSettings(token) => call!(service.catalog_settings(token), Msg::Settings),
        Effect::LoadProducts {
            token,
            query,
            append,
        } => {
            let append = *append;
            call!(service.products(token, query), move |r| Msg::Products(
                append, r
            ))
        }
        Effect::LoadProduct { token, id } => call!(service.product(token, id), |r: Result<
            sultan_client::Product,
            _,
        >| {
            Msg::Product(r.map(Box::new))
        }),
        Effect::LoadBlocks { token, id } => {
            let id = id.clone();
            call!(service.branch_blocks(token, &id), move |r| Msg::Blocks(
                id, r
            ))
        }
        Effect::LoadImage { token, id } => {
            let id = id.clone();
            call!(service.product_image(token, &id), move |r| Msg::Image(
                id, r
            ))
        }
        Effect::SaveProduct { token, id, body } => {
            call!(service.put_product(token, id, body), |r: Result<
                sultan_client::Product,
                _,
            >| {
                Msg::Saved(r.map(Box::new))
            })
        }
        Effect::PutBlocks {
            token,
            id,
            branch_ids,
        } => call!(
            service.put_branch_blocks(token, id, branch_ids),
            Msg::BlocksSaved
        ),
        Effect::PutImage { token, id, bytes } => call!(
            service.put_product_image(token, id, bytes.clone()),
            Msg::ImageSaved
        ),
        Effect::DeleteImage { token, id } => {
            call!(service.delete_product_image(token, id), Msg::ImageDeleted)
        }
        Effect::SetArchived {
            token,
            id,
            archived,
            key,
        } => call!(
            service.set_archived(token, id, *archived, key),
            |r: Result<sultan_client::Product, _>| Msg::Archived(r.map(Box::new))
        ),
        Effect::Generate { token, what, key } => {
            call!(service.generate(token, *what, key), Msg::Generated)
        }
        Effect::Search { token, q } => call!(
            service.search(token, q, crate::backoffice::SEARCH_LIMIT),
            Msg::Hits
        ),
        Effect::LoadComponent { token, id } => call!(service.product(token, id), |r: Result<
            sultan_client::Product,
            _,
        >| {
            Msg::Component(r.map(Box::new))
        }),
        Effect::CheckProduct { token, id } => call!(service.product(token, id), |r: Result<
            sultan_client::Product,
            _,
        >| {
            Msg::Checked(r.map(Box::new))
        }),
        Effect::PutCategory { token, id, body } => {
            call!(service.put_category(token, id, body), Msg::CategorySaved)
        }
        Effect::PutBrand { token, id, body } => {
            call!(service.put_brand(token, id, body), Msg::BrandSaved)
        }
        Effect::Logout(token) => call!(service.logout(token), Msg::LoggedOut),
        Effect::WatchEvents { .. } | Effect::Debounce(_) | Effect::ReadFile(_) | Effect::Exit => {
            None
        }
    }
}

impl State {
    fn dispatch(&self, msg: Msg) -> bool {
        let step = self.controller.borrow_mut().update(msg);
        self.run(step.effects);
        self.drain();
        self.render();
        step.handled
    }

    fn drain(&self) {
        if self.draining.replace(true) {
            return;
        }
        loop {
            let next = self
                .inbox
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .pop_front();
            let Some(msg) = next else { break };
            let step = self.controller.borrow_mut().update(msg);
            self.run(step.effects);
        }
        self.draining.set(false);
    }

    fn run(&self, effects: Vec<Effect>) {
        for effect in effects {
            match &effect {
                Effect::WatchEvents { token, generation } => {
                    let inbox = self.inbox.clone();
                    let wake = self.wake.clone();
                    let generation = *generation;
                    let sink: EventSink = Arc::new(move |event| {
                        push(&inbox, Msg::Event(generation, event));
                        wake();
                    });
                    let watching = self.service.watch_events(token, sink);
                    // Ends when the socket closes (logout) or fails; nothing to report.
                    let task = self.runner.handle().spawn(async move {
                        let _ = watching.await;
                    });
                    self.watchers.replace(task.abort_handle());
                    continue;
                }
                Effect::Debounce(serial) => {
                    self.debounce(*serial);
                    continue;
                }
                Effect::ReadFile(path) => {
                    let read = std::fs::read(path).map_err(|e| e.to_string());
                    push(&self.inbox, Msg::FileRead(read));
                    continue;
                }
                Effect::Exit => {
                    if let Some(ui) = self.ui.upgrade() {
                        // Ends `run_event_loop` when this is the last window.
                        let _ = ui.hide();
                    }
                    continue;
                }
                _ => {}
            }
            let Some(future) = start(self.service.as_ref(), &effect) else {
                continue;
            };
            match &self.runner {
                Runner::Block(handle) => push(&self.inbox, handle.block_on(future)),
                Runner::Spawn(handle) => {
                    let inbox = self.inbox.clone();
                    let wake = self.wake.clone();
                    handle.spawn(async move {
                        push(&inbox, future.await);
                        wake();
                    });
                }
            }
        }
    }

    fn debounce(&self, serial: u64) {
        if self.options.debounce.is_zero() {
            push(&self.inbox, Msg::SearchDue(serial));
            return;
        }
        let timer = slint::Timer::default();
        let inbox = self.inbox.clone();
        let wake = self.wake.clone();
        timer.start(
            slint::TimerMode::SingleShot,
            self.options.debounce,
            move || {
                push(&inbox, Msg::SearchDue(serial));
                wake();
            },
        );
        *self.debounce.borrow_mut() = Some(timer);
    }

    fn render(&self) {
        let Some(ui) = self.ui.upgrade() else { return };
        let c = self.controller.borrow();
        render(&ui, &c, self);
    }

    /// The thumbnail of the open form (decoded once per image change).
    fn thumbnail(&self, c: &Controller) -> slint::Image {
        let Some(f) = &c.form else {
            return slint::Image::default();
        };
        let mut cached = self.thumbnail.borrow_mut();
        if cached.0 != f.form.id || cached.1 != f.image_serial {
            let image = f.image.as_deref().map(decode).unwrap_or_default();
            *cached = (f.form.id.clone(), f.image_serial, image);
        }
        cached.2.clone()
    }
}

/// Decodes PNG, JPEG or WebP bytes into a `slint::Image` (empty when they do not decode).
#[must_use]
pub fn decode(bytes: &[u8]) -> slint::Image {
    let Ok(image) = image::load_from_memory(bytes) else {
        return slint::Image::default();
    };
    let rgba = image.to_rgba8();
    let buffer = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
        rgba.as_raw(),
        rgba.width(),
        rgba.height(),
    );
    slint::Image::from_rgba8(buffer)
}

fn note(notice: Option<&Notice>) -> Note {
    notice.map_or_else(Note::default, |n| Note {
        code: n.code.into(),
        arg: n.arg.as_str().into(),
        tone: n.tone.value(),
    })
}

fn error_note(error: Option<&FormError>) -> Note {
    error.map_or_else(Note::default, |e| Note {
        code: e.code.as_str().into(),
        arg: e.arg.as_str().into(),
        tone: -1,
    })
}

/// The first error of `field` that names no row (shown above the rows).
fn area_error<'a>(form: &'a ProductForm, fields: &[&str]) -> Option<&'a FormError> {
    form.errors
        .iter()
        .find(|e| e.row.is_none() && fields.iter().any(|f| e.is_for(f, None)))
}

fn index(i: usize) -> i32 {
    i32::try_from(i).unwrap_or(i32::MAX)
}

fn position<T: PartialEq>(items: impl IntoIterator<Item = T>, wanted: &T) -> i32 {
    items
        .into_iter()
        .position(|x| x == *wanted)
        .map_or(0, index)
}

fn strings(labels: impl IntoIterator<Item = String>) -> Vec<SharedString> {
    labels.into_iter().map(SharedString::from).collect()
}

/// Choice labels with the first (empty) one replaced by the translated `none`.
fn choice_labels(
    ui: &BackOfficeWindow,
    choices: &[(Option<String>, String)],
    none: &str,
) -> Vec<SharedString> {
    choices
        .iter()
        .map(|(id, label)| {
            if id.is_none() {
                ui.invoke_label(none.into())
            } else {
                label.as_str().into()
            }
        })
        .collect()
}

#[allow(clippy::too_many_lines)] // one assignment per window property
fn render(ui: &BackOfficeWindow, c: &Controller, state: &State) {
    let models = &state.models;
    let screen = c.screen();
    ui.set_screen(screen.name().into());
    ui.set_busy(c.busy || c.list.loading || c.form.as_ref().is_some_and(|f| f.saving));
    ui.set_server(state.options.server.as_str().into());
    ui.set_user_name(
        c.session
            .as_ref()
            .map(|s| s.display_name.as_str())
            .unwrap_or_default()
            .into(),
    );
    let read_only = c
        .session
        .as_ref()
        .and_then(crate::backoffice::Session::read_only_reason)
        .map(|code| Notice {
            code,
            arg: String::new(),
            tone: crate::controller::Tone::Neutral,
            until: None,
        });
    ui.set_read_only(note(read_only.as_ref()));
    ui.set_login(BoLogin {
        server: state.options.server.as_str().into(),
        username: c.login.username.as_str().into(),
        notice: note(c.login.notice.as_ref()),
    });
    if state.focus_seen.get() != c.focus_serial || screen != Screen::Form {
        ui.set_focus_target(c.focus.unwrap_or_default().into());
        ui.set_focus_serial(
            i32::try_from((c.focus_serial + state.focus_bumps.get()) % 1_000_000).unwrap_or(0),
        );
    }
    ui.set_confirm(
        match c.confirm {
            None => "",
            Some(Confirm::Discard(_)) => "discard",
            Some(Confirm::Archive) if c.form.as_ref().is_some_and(|f| f.form.archived) => {
                "unarchive"
            }
            Some(Confirm::Archive) => "archive",
        }
        .into(),
    );

    // Product list.
    let r = &c.reference;
    let filter_categories = r.category_choices(None);
    let filter_brands = r.brand_choices(None);
    sync(
        &models.filter_categories,
        choice_labels(ui, &filter_categories, "all_categories"),
    );
    sync(
        &models.filter_brands,
        choice_labels(ui, &filter_brands, "all_brands"),
    );
    let l = &c.list;
    ui.set_list(BoList {
        search: l.search.as_str().into(),
        category_index: position(filter_categories.iter().map(|x| &x.0), &&l.category),
        brand_index: position(filter_brands.iter().map(|x| &x.0), &&l.brand),
        type_index: l
            .product_type
            .and_then(|t| TYPES.iter().position(|x| *x == t))
            .map_or(0, |i| index(i + 1)),
        archived: l.archived,
        show_archived: c.can_view_archived(),
        can_edit: c.can_manage(),
        cursor: index(l.cursor),
        has_more: l.next.is_some(),
        loading: l.loading,
        notice: note(l.notice.as_ref()),
    });
    sync(
        &models.rows,
        l.rows
            .iter()
            .map(|p| BoProductRow {
                code: p.code.as_str().into(),
                name: p.name.as_str().into(),
                kind: p.product_type.as_str().into(),
                category: r.category_name(p.category_id.as_deref()).into(),
                brand: r.brand_name(p.brand_id.as_deref()).into(),
                variants: p.variant_count.to_string().into(),
                archived: p.archived_at.is_some(),
            })
            .collect(),
    );

    // Taxonomy.
    sync(
        &models.category_rows,
        r.categories
            .iter()
            .map(|row| BoTreeRow {
                name: row.category.name.as_str().into(),
                code: row.category.code.as_str().into(),
                depth: index(row.depth),
                archived: row.category.archived,
            })
            .collect(),
    );
    let parents = c.parent_choices();
    sync(
        &models.parent_choices,
        choice_labels(ui, &parents, "top_level"),
    );
    let ce = &c.categories;
    ui.set_category_editor(BoCategoryEditor {
        is_new: ce.editing.is_none(),
        name: ce.name.as_str().into(),
        code: ce.code.as_str().into(),
        parent_index: position(parents.iter().map(|x| &x.0), &&ce.parent),
        archived: ce
            .editing
            .as_ref()
            .and_then(|id| r.categories.iter().find(|x| x.category.id == *id))
            .is_some_and(|x| x.category.archived),
        cursor: index(ce.cursor),
        can_edit: c.can_manage(),
        notice: note(ce.notice.as_ref()),
    });
    sync(
        &models.brand_rows,
        r.brands
            .iter()
            .map(|b| BoTreeRow {
                name: b.name.as_str().into(),
                code: SharedString::new(),
                depth: 0,
                archived: b.archived,
            })
            .collect(),
    );
    let be = &c.brands;
    ui.set_brand_editor(BoBrandEditor {
        is_new: be.editing.is_none(),
        name: be.name.as_str().into(),
        archived: be
            .editing
            .as_ref()
            .and_then(|id| r.brands.iter().find(|b| b.id == *id))
            .is_some_and(|b| b.archived),
        cursor: index(be.cursor),
        can_edit: c.can_manage(),
        notice: note(be.notice.as_ref()),
    });

    if screen == Screen::Form
        && let Some(f) = &c.form
    {
        render_form(ui, c, f, state);
    }
}

#[allow(clippy::too_many_lines)] // one assignment per form property
fn render_form(
    ui: &BackOfficeWindow,
    c: &Controller,
    f: &crate::backoffice::FormState,
    state: &State,
) {
    let models = &state.models;
    let form = &f.form;
    let d = &form.data;
    let r = &c.reference;
    let can_edit = c.can_manage();

    // Rebuild the form when rows were added or removed (see the module docs).
    let shape: Vec<usize> = [
        d.options.len(),
        d.combos.len(),
        d.units.len(),
        d.barcodes.len(),
        d.components.len(),
        f.hits.len(),
    ]
    .into_iter()
    .chain(d.options.iter().map(|o| o.values.len()))
    .collect();
    if *state.shape.borrow() != shape {
        *state.shape.borrow_mut() = shape;
        state.rebuilds.set(state.rebuilds.get() + 1);
        // The rebuild drops the focused field: give the focus back to the window (unless the
        // controller asked for a field in this same step).
        if state.focus_seen.get() == c.focus_serial {
            state.focus_bumps.set(state.focus_bumps.get() + 1);
            ui.set_focus_target("list".into());
            ui.set_focus_serial(
                i32::try_from((c.focus_serial + state.focus_bumps.get()) % 1_000_000).unwrap_or(0),
            );
        }
    }
    state.focus_seen.set(c.focus_serial);
    let serial = c.form_serial + state.rebuilds.get();
    ui.set_form_serial(i32::try_from(serial % 1_000_000).unwrap_or(0));

    let categories = r.category_choices(d.category_id.as_deref());
    let brands = r.brand_choices(d.brand_id.as_deref());
    sync(
        &models.category_choices,
        choice_labels(ui, &categories, "no_category"),
    );
    sync(
        &models.brand_choices,
        choice_labels(ui, &brands, "no_brand"),
    );
    sync(
        &models.tax_choices,
        strings(r.tax_classes.iter().map(|t| t.name.clone())),
    );
    let units = form.active_units();
    sync(
        &models.unit_choices,
        strings(units.iter().map(|u| u.1.clone())),
    );
    let combos = form.active_combos();
    sync(
        &models.variant_choices,
        strings(
            combos
                .iter()
                .map(|i| form.variant_label(&d.combos[*i].variant_id)),
        ),
    );
    sync(
        &models.tab_errors,
        Tab::ALL.iter().map(|t| form.tab_has_errors(*t)).collect(),
    );
    let err = |field: &str| error_note(form.error_for(field, None));
    let unit_ids: Vec<&String> = units.iter().map(|u| &u.0).collect();

    ui.set_general(BoGeneral {
        code: d.code.as_str().into(),
        code_placeholder: format!("{}000123", r.settings.product_code_prefix).into(),
        name: d.name.as_str().into(),
        receipt: d.receipt_name.as_str().into(),
        receipt_placeholder: form.receipt_placeholder().into(),
        type_index: position(TYPES, &d.product_type),
        type_locked: form.used,
        category_index: position(categories.iter().map(|x| &x.0), &&d.category_id),
        brand_index: position(brands.iter().map(|x| &x.0), &&d.brand_id),
        tax_index: position(r.tax_classes.iter().map(|t| &t.id), &&d.tax_class_id),
        tags: d.tags.as_str().into(),
        open_price: d.open_price,
        requires_note: d.requires_note,
        sellable: d.sellable,
        purchasable: d.purchasable,
        code_error: err("code"),
        name_error: err("name"),
        receipt_error: err("receipt_name"),
        type_error: error_note(
            form.error_for("type", None)
                .or_else(|| form.error_for("tracking", None)),
        ),
        tags_error: err("tags"),
        reference_error: error_note(
            form.error_for("category_id", None)
                .or_else(|| form.error_for("brand_id", None))
                .or_else(|| form.error_for("tax_class_id", None)),
        ),
    });

    let base = &d.units[0];
    ui.set_form(BoForm {
        title: d.name.as_str().into(),
        code: d.code.as_str().into(),
        is_new: form.is_new,
        used: form.used,
        archived: form.archived,
        dirty: form.is_dirty(),
        can_edit,
        saving: f.saving,
        tab: index(form.tab.index()),
        tab_errors: ModelRc::from(models.tab_errors.clone()),
        notice: note(f.notice.as_ref()),
        changed_elsewhere: f.changed_elsewhere,
        has_options: !d.options.is_empty(),
        can_add_option: form.can_add_option(),
        weighed: d.product_type == ProductType::Weighed,
        bundle: d.product_type == ProductType::Bundle,
        options_error: error_note(area_error(form, &["options"])),
        variants_error: error_note(area_error(form, &["variants"])),
        base_name: base.name.as_str().into(),
        base_short: base.short_name.as_str().into(),
        kind_index: i32::from(d.base_kind == UnitKind::Decimal),
        kind_locked: form.used
            || matches!(d.product_type, ProductType::Weighed | ProductType::Bundle),
        base_error: error_note(
            form.error_for("units", Some(0))
                .or_else(|| form.error_for("base_unit", None)),
        ),
        can_add_unit: form.can_add_unit(),
        units_error: error_note(area_error(form, &["units"])),
        sale_index: position(unit_ids.iter().copied(), &&d.default_sale),
        purchase_index: position(unit_ids.iter().copied(), &&d.default_purchase),
        defaults_error: error_note(
            form.error_for("default_sale_unit_id", None)
                .or_else(|| form.error_for("default_purchase_unit_id", None)),
        ),
        pick_variant: index(f.pick_variant),
        pick_unit: index(f.pick_unit),
        new_barcode: f.new_barcode.as_str().into(),
        barcodes_error: error_note(area_error(form, &["barcodes"])),
        component_search: f.component_search.as_str().into(),
        components_error: error_note(area_error(form, &["components"])),
        image_path: f.image_path.as_str().into(),
        has_image: f.image.is_some(),
    });
    ui.set_thumbnail(state.thumbnail(c));

    // Options with their values (one value model per option, updated row by row).
    {
        let mut values = state.models.values.borrow_mut();
        values.resize_with(d.options.len(), Rc::default);
        let rows: Vec<BoOption> = d
            .options
            .iter()
            .enumerate()
            .map(|(i, o)| {
                let value_rows: Vec<BoValue> = o
                    .values
                    .iter()
                    .enumerate()
                    .map(|(j, v)| BoValue {
                        name: v.name.as_str().into(),
                        code: v.code.as_str().into(),
                        can_remove: form.can_remove_value(i, j),
                    })
                    .collect();
                sync_rows(&values[i], &value_rows);
                BoOption {
                    name: o.name.as_str().into(),
                    values: ModelRc::from(values[i].clone()),
                    error: error_note(form.error_for("options", Some(i))),
                }
            })
            .collect();
        sync_rows(&models.options, &rows);
    }
    let combo_rows: Vec<BoCombo> = d
        .combos
        .iter()
        .enumerate()
        .map(|(i, combo)| BoCombo {
            label: form.combo_label(combo).into(),
            on: combo.on,
            sku: combo.sku.as_str().into(),
            sku_default: form.default_sku(i).into(),
            plu: combo.plu.as_str().into(),
            stored: combo.stored,
            error: error_note(form.error_for("variants", Some(i))),
        })
        .collect();
    sync_rows(&models.combos, &combo_rows);
    let unit_rows: Vec<BoUnit> = d
        .units
        .iter()
        .enumerate()
        .skip(1)
        .map(|(i, u)| BoUnit {
            name: u.name.as_str().into(),
            short_name: u.short_name.as_str().into(),
            factor: u.factor.as_str().into(),
            sellable: u.sellable,
            purchasable: u.purchasable,
            archived: u.archived,
            factor_locked: form.factor_locked(i),
            archive_instead: form.used && u.stored,
            error: error_note(form.error_for("units", Some(i))),
        })
        .collect();
    sync_rows(&models.units, &unit_rows);
    let unit_name = |id: &str| {
        d.units
            .iter()
            .find(|u| u.id == id)
            .map(|u| u.name.clone())
            .unwrap_or_default()
    };
    let barcode_rows: Vec<BoBarcode> = d
        .barcodes
        .iter()
        .enumerate()
        .map(|(i, b)| BoBarcode {
            label: format!(
                "{} · {}",
                form.variant_label(&b.variant_id),
                unit_name(&b.unit_id)
            )
            .into(),
            code: b.code.as_str().into(),
            error: error_note(form.error_for("barcodes", Some(i))),
        })
        .collect();
    sync_rows(&models.barcodes, &barcode_rows);
    let component_rows: Vec<BoComponent> = d
        .components
        .iter()
        .enumerate()
        .map(|(i, comp)| BoComponent {
            name: comp.name.as_str().into(),
            sku: comp.sku.as_str().into(),
            units: ModelRc::new(VecModel::from(if comp.units.is_empty() {
                vec![SharedString::from("…")]
            } else {
                strings(comp.units.iter().map(|u| u.1.clone()))
            })),
            unit_index: position(comp.units.iter().map(|u| &u.0), &&comp.unit_id),
            qty: comp.qty.as_str().into(),
            error: error_note(form.error_for("components", Some(i))),
        })
        .collect();
    // Unit lists are fresh models: compare without them.
    let same = models.components.row_count() == component_rows.len()
        && models.components.iter().zip(&component_rows).all(|(a, b)| {
            a.name == b.name
                && a.sku == b.sku
                && a.unit_index == b.unit_index
                && a.qty == b.qty
                && a.error == b.error
                && a.units.row_count() == b.units.row_count()
        });
    if !same {
        sync_rows(&models.components, &component_rows);
    }
    sync(
        &models.hits,
        f.hits
            .iter()
            .map(|h| BoHit {
                name: h.name.as_str().into(),
                sku: h.sku.as_str().into(),
            })
            .collect(),
    );
    sync(
        &models.branches,
        r.branches
            .iter()
            .map(|b| BoBranch {
                label: format!("{} · {}", b.code, b.name).into(),
                sold: !d.blocked.contains(&b.id),
            })
            .collect(),
    );
}

/// Wires the window's callbacks to `state` (weakly: the window must not keep it alive).
fn connect(ui: &BackOfficeWindow, state: &Rc<State>) {
    let weak = || Rc::downgrade(state);
    ui.on_capture_key({
        let s = weak();
        move |text, ctrl, shift| {
            s.upgrade()
                .is_some_and(|s| s.dispatch(Msg::Capture(Press::parse(&text, ctrl, shift))))
        }
    });
    ui.on_key({
        let s = weak();
        move |text, ctrl, shift| {
            s.upgrade()
                .is_some_and(|s| s.dispatch(Msg::Key(Press::parse(&text, ctrl, shift))))
        }
    });
    ui.on_action({
        let s = weak();
        move |id| {
            if let Some(s) = s.upgrade() {
                s.dispatch(Msg::Action(id.into()));
            }
        }
    });
    ui.on_edit({
        let s = weak();
        move |kind, row, sub, field, value| {
            let (Some(s), Ok(row), Ok(sub)) =
                (s.upgrade(), usize::try_from(row), usize::try_from(sub))
            else {
                return;
            };
            s.dispatch(Msg::Edit {
                kind: kind.into(),
                row,
                sub,
                field: field.into(),
                value: value.into(),
            });
        }
    });
    ui.on_toggle({
        let s = weak();
        move |kind, row, field, on| {
            let (Some(s), Ok(row)) = (s.upgrade(), usize::try_from(row)) else {
                return;
            };
            s.dispatch(Msg::Toggle {
                kind: kind.into(),
                row,
                field: field.into(),
                on,
            });
        }
    });
    ui.on_choose({
        let s = weak();
        move |kind, row, field, i| {
            let (Some(s), Ok(row), Ok(index)) =
                (s.upgrade(), usize::try_from(row), usize::try_from(i))
            else {
                return;
            };
            s.dispatch(Msg::Choose {
                kind: kind.into(),
                row,
                field: field.into(),
                index,
            });
        }
    });
    ui.on_pick({
        let s = weak();
        move |kind, row, sub| {
            let (Some(s), Ok(row), Ok(sub)) =
                (s.upgrade(), usize::try_from(row), usize::try_from(sub))
            else {
                return;
            };
            s.dispatch(Msg::Pick {
                kind: kind.into(),
                row,
                sub,
            });
        }
    });
    ui.on_pump({
        let s = weak();
        move || {
            if let Some(s) = s.upgrade() {
                s.drain();
                s.render();
            }
        }
    });
}

/// Builds the back-office window at the login screen.
///
/// # Errors
/// The Slint platform error (no display, no backend).
pub fn build_backoffice(
    service: Arc<dyn BackOffice>,
    runner: Runner,
    options: BackOfficeOptions,
) -> Result<BackOfficeApp, slint::PlatformError> {
    let ui = BackOfficeWindow::new()?;
    if let Some(language) = &options.language {
        // `en` (or "") is the source language; an unknown language falls back to it.
        if slint::select_bundled_translation(language).is_err() {
            let _ = slint::select_bundled_translation("en");
        }
    }
    let models = Models::default();
    ui.set_rows(ModelRc::from(models.rows.clone()));
    ui.set_filter_categories(ModelRc::from(models.filter_categories.clone()));
    ui.set_filter_brands(ModelRc::from(models.filter_brands.clone()));
    ui.set_options(ModelRc::from(models.options.clone()));
    ui.set_combos(ModelRc::from(models.combos.clone()));
    ui.set_units(ModelRc::from(models.units.clone()));
    ui.set_barcodes(ModelRc::from(models.barcodes.clone()));
    ui.set_components(ModelRc::from(models.components.clone()));
    ui.set_hits(ModelRc::from(models.hits.clone()));
    ui.set_branches(ModelRc::from(models.branches.clone()));
    ui.set_category_choices(ModelRc::from(models.category_choices.clone()));
    ui.set_brand_choices(ModelRc::from(models.brand_choices.clone()));
    ui.set_tax_choices(ModelRc::from(models.tax_choices.clone()));
    ui.set_unit_choices(ModelRc::from(models.unit_choices.clone()));
    ui.set_variant_choices(ModelRc::from(models.variant_choices.clone()));
    ui.set_category_rows(ModelRc::from(models.category_rows.clone()));
    ui.set_parent_choices(ModelRc::from(models.parent_choices.clone()));
    ui.set_brand_rows(ModelRc::from(models.brand_rows.clone()));

    let wake: Wake = match &runner {
        Runner::Spawn(_) => {
            let weak = ui.as_weak();
            Arc::new(move || {
                let _ = weak.upgrade_in_event_loop(|ui| ui.invoke_pump());
            })
        }
        Runner::Block(_) => Arc::new(|| {}),
    };
    let mut controller = Controller::new(&options.username);
    controller.start = options.start;
    ui.set_start(options.start);
    let state = Rc::new(State {
        watchers: Watchers::default(),
        ui: ui.as_weak(),
        controller: RefCell::new(controller),
        inbox: Inbox::default(),
        wake,
        service,
        runner,
        options,
        models,
        draining: Cell::new(false),
        shape: RefCell::default(),
        rebuilds: Cell::new(0),
        focus_seen: Cell::new(0),
        focus_bumps: Cell::new(0),
        thumbnail: RefCell::new((String::new(), 0, slint::Image::default())),
        debounce: RefCell::default(),
    });
    connect(&ui, &state);
    state.render();
    Ok(BackOfficeApp { ui, state })
}

/// Opens the back-office window and runs the event loop until it is closed.
///
/// # Errors
/// The Slint platform error.
pub fn run_backoffice(
    service: Arc<dyn BackOffice>,
    handle: Handle,
    options: BackOfficeOptions,
) -> Result<(), slint::PlatformError> {
    let app = build_backoffice(service, Runner::Spawn(handle), options)?;
    let result = app.ui.run();
    // The window is closed: its event socket must not outlive it.
    app.state.watchers.stop();
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_sync_in_place() {
        let model = VecModel::from(vec![1, 2, 3]);
        sync_rows(&model, &[1, 5]);
        assert_eq!(model.iter().collect::<Vec<_>>(), [1, 5]);
        sync_rows(&model, &[1, 5, 7, 8]);
        assert_eq!(model.iter().collect::<Vec<_>>(), [1, 5, 7, 8]);
        assert_eq!(position([1, 2, 3], &3), 2);
        assert_eq!(position([1, 2, 3], &9), 0, "unknown → first");
        assert_eq!(decode(b"not an image").size().width, 0);
        assert_eq!(index(usize::MAX), i32::MAX);
    }
}
