use serde_json::json;
use sultan_client::catalog::{CATALOG_MANAGE, CATALOG_VIEW_ARCHIVED};
use sultan_client::{Problem, ProductSummary};

use super::*;

const TAX: &str = "00000000-0000-7000-8000-000000000101";

fn press(text: &str, ctrl: bool, shift: bool) -> Msg {
    Msg::Capture(Press::parse(text, ctrl, shift))
}

fn ctrl(c: &str) -> Msg {
    press(c, true, false)
}

const ESC: &str = "\u{1b}";
const ENTER: &str = "\n";
const UP: &str = "\u{F700}";
const DOWN: &str = "\u{F701}";

fn f(n: u32) -> String {
    char::from_u32(0xF704 + n - 1).unwrap().to_string()
}

fn edit(kind: &str, row: usize, sub: usize, field: &str, value: &str) -> Msg {
    Msg::Edit {
        kind: kind.into(),
        row,
        sub,
        field: field.into(),
        value: value.into(),
    }
}

fn action(id: &str) -> Msg {
    Msg::Action(id.into())
}

fn pick(kind: &str, row: usize, sub: usize) -> Msg {
    Msg::Pick {
        kind: kind.into(),
        row,
        sub,
    }
}

fn toggle(kind: &str, row: usize, field: &str, on: bool) -> Msg {
    Msg::Toggle {
        kind: kind.into(),
        row,
        field: field.into(),
        on,
    }
}

fn choose(kind: &str, row: usize, field: &str, index: usize) -> Msg {
    Msg::Choose {
        kind: kind.into(),
        row,
        field: field.into(),
        index,
    }
}

fn me(everywhere: &[&str], here: &[&str]) -> Me {
    Me {
        user_id: "u".into(),
        username: "owner".into(),
        display_name: "Owner".into(),
        branch_id: "b".into(),
        register_id: None,
        permissions: here.iter().map(|s| (*s).to_owned()).collect(),
        permissions_everywhere: everywhere.iter().map(|s| (*s).to_owned()).collect(),
        permissions_anywhere: Vec::new(),
    }
}

/// A product ID (a UUID, as the form checks it).
fn pid(n: usize) -> String {
    format!("00000000-0000-7000-8000-{n:012}")
}

fn summary(n: usize) -> ProductSummary {
    ProductSummary {
        id: pid(n),
        code: format!("P{n}"),
        name: format!("Produk {n}"),
        product_type: "stock".into(),
        category_id: None,
        brand_id: None,
        sellable: true,
        archived_at: None,
        variant_count: 1,
    }
}

fn product(id: &str, updated_at: i64) -> Box<Product> {
    let (pcs, v) = (new_id(), new_id());
    Box::new(
        serde_json::from_value(json!({
            "id": id, "code": "TEH", "name": "Teh", "type": "stock", "tax_class_id": TAX,
            "units": [{ "id": pcs, "name": "pcs", "kind": "integer", "factor_thousandths": 1000 }],
            "base_unit_id": pcs, "default_sale_unit_id": pcs, "default_purchase_unit_id": pcs,
            "variants": [{ "id": v, "sku": "TEH" }],
            "updated_at": updated_at,
        }))
        .unwrap(),
    )
}

/// A bundle of one line of product `other` (labelled like the server's answer).
fn bundle(id: &str) -> Box<Product> {
    let mut p = product(id, 1);
    p.product_type = "bundle".into();
    p.components = vec![sultan_client::catalog::Component {
        variant_id: new_id(),
        unit_id: pid(99),
        qty_thousandths: 1000,
        product_id: Some("other".into()),
        name: Some("Teh – Manis".into()),
        sku: Some("TEH-M".into()),
        unit_name: Some("pcs".into()),
    }];
    p
}

fn api(status: u16, code: &str) -> ClientError {
    ClientError::Api(Problem::new(status, code, code))
}

fn notice_code(c: &Controller) -> &'static str {
    c.form
        .as_ref()
        .and_then(|f| f.notice.as_ref())
        .map_or("", |n| n.code)
}

/// Logged in as a user with these permissions, at the product list with 3 rows.
fn logged_in(everywhere: &[&str], here: &[&str]) -> Controller {
    let mut c = Controller::new("owner");
    c.update(edit("login", 0, 0, "password", "pw"));
    let effects = c.update(action("login")).effects;
    assert!(matches!(&effects[..], [Effect::Login { username, .. }] if username == "owner"));
    let effects = c
        .update(Msg::LoggedIn(Ok(Login {
            token: "t".into(),
            user_id: "u".into(),
            display_name: "Owner".into(),
            branch_id: "b".into(),
            register_id: None,
            expires_at: 0,
        })))
        .effects;
    assert!(effects.contains(&Effect::LoadMe("t".into())));
    assert!(effects.contains(&Effect::WatchEvents {
        token: "t".into(),
        generation: 1
    }));
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::LoadProducts { append: false, .. }))
    );
    let effects = c.update(Msg::Me(Ok(me(everywhere, here)))).effects;
    assert_eq!(effects.len(), 2, "categories and brands");
    c.update(Msg::TaxClasses(Ok(vec![TaxClass {
        id: TAX.into(),
        name: "Standard".into(),
        archived: false,
    }])));
    c.update(Msg::Branches(Ok(vec![Branch {
        id: "b1".into(),
        code: "JKT01".into(),
        name: "Jakarta".into(),
        timezone: String::new(),
    }])));
    c.update(Msg::Settings(Ok(CatalogSettings::default())));
    c.update(Msg::Products(
        false,
        Ok(ProductPage {
            items: (0..3).map(summary).collect(),
            next_before_id: Some(pid(2)),
        }),
    ));
    assert_eq!(c.screen(), Screen::Products);
    c
}

fn owner() -> Controller {
    logged_in(&[CATALOG_MANAGE, CATALOG_VIEW_ARCHIVED], &[CATALOG_MANAGE])
}

/// The owner with product `p01` open.
fn with_form() -> Controller {
    let mut c = owner();
    c.update(pick("open", 1, 0));
    let effects = c.update(Msg::Product(Ok(product(&pid(1), 5)))).effects;
    assert_eq!(effects.len(), 2, "blocks and image");
    assert_eq!(c.screen(), Screen::Form);
    c
}

#[test]
fn keys_parse() {
    let cases = [
        ("a", false, PressKey::Char('a')),
        ("A", true, PressKey::Char('a')),
        ("/", false, PressKey::Char('/')),
        (ENTER, false, PressKey::Enter),
        ("\r", false, PressKey::Enter),
        (ESC, false, PressKey::Escape),
        (UP, false, PressKey::Up),
        (DOWN, false, PressKey::Down),
        ("\u{F708}", false, PressKey::F(5)),
        ("\u{8}", false, PressKey::Other),
        ("ab", false, PressKey::Other),
        ("", false, PressKey::Other),
    ];
    for (text, shift, key) in cases {
        assert_eq!(Press::parse(text, false, shift).key, key, "{text:?}");
    }
    assert!(Press::parse("s", true, false).ctrl_char('s'));
    assert!(!Press::parse("s", true, true).ctrl_char('s'));
    assert!(Press::parse("A", true, true).ctrl_shift_char('a'));
    assert!(!Press::parse(ENTER, true, false).plain(PressKey::Enter));
    let names = [
        Screen::Login,
        Screen::Products,
        Screen::Form,
        Screen::Categories,
        Screen::Brands,
    ]
    .map(Screen::name);
    assert_eq!(names, ["login", "products", "form", "categories", "brands"]);
}

#[test]
fn notices_for_errors() {
    let cases = [
        (api(401, "invalid_credentials"), "wrong_password"),
        (api(423, "locked_out"), "locked_out_bo"),
        (api(401, "invalid_session"), "session_ended"),
        (api(403, "permission_denied"), "permission_denied_catalog"),
        (api(403, "read_only_replica"), "read_only_replica"),
        (api(422, "invalid_image"), "invalid_image"),
        (api(413, "payload_too_large"), "image_too_large"),
        (api(409, "conflict"), "conflict_retry"),
        (api(404, "not_found"), "not_found"),
        (api(422, "duplicate_brand_name"), "duplicate_name"),
        (api(422, "category_cycle"), "category_cycle"),
        (api(422, "category_too_deep"), "category_too_deep"),
        (api(422, "invalid_field"), "invalid_input"),
        (api(500, "internal"), "error"),
        (ClientError::Network("x".into()), "network"),
        (ClientError::Unexpected("x".into()), "error"),
    ];
    for (error, code) in cases {
        assert_eq!(notice_for(&error).code, code, "{error:?}");
    }
    assert_eq!(
        notice_for(&api(403, "permission_denied")).arg,
        CATALOG_MANAGE
    );
}

#[test]
fn login_needs_both_fields_and_shows_failures() {
    let mut c = Controller::new("");
    assert_eq!((c.screen(), c.focus), (Screen::Login, Some("username")));
    assert!(c.update(action("login")).effects.is_empty());
    assert_eq!(c.login.notice.as_ref().unwrap().code, "login_required");
    c.update(edit("login", 0, 0, "username", "owner"));
    c.update(edit("login", 0, 0, "password", "x"));
    c.update(edit("login", 0, 0, "other", "x"));
    assert_eq!(c.update(action("login")).effects.len(), 1);
    assert!(c.busy);
    assert!(
        c.update(action("login")).effects.is_empty(),
        "no double login"
    );
    assert!(c.login.password.is_empty(), "the password is not kept");
    c.update(Msg::LoggedIn(Err(api(401, "invalid_credentials"))));
    assert!(!c.busy);
    assert_eq!(c.login.notice.as_ref().unwrap().code, "wrong_password");
    assert_eq!(c.focus, Some("password"));
    // Keys do nothing on the login screen; F2 neither.
    assert_eq!(c.update(press(&f(2), false, false)), Step::default());
}

#[test]
fn read_only_users_can_browse_but_not_write() {
    // Since C3b a grant at one branch is enough (spec 2.2 §11).
    let branch_manager = logged_in(&[], &[CATALOG_MANAGE, CATALOG_VIEW_ARCHIVED]);
    assert!(branch_manager.can_manage() && branch_manager.can_view_archived());
    let mut anywhere = me(&[], &[]);
    anywhere.permissions_anywhere = vec![CATALOG_MANAGE.into()];
    let session = Session {
        token: "t".into(),
        me: anywhere,
        display_name: "X".into(),
    };
    assert!(session.can_manage() && session.read_only_reason().is_none());

    let mut c = logged_in(&[], &[]);
    let session = c.session.clone().unwrap();
    assert_eq!(session.read_only_reason(), Some("read_only"));
    assert!(!c.can_manage() && !c.can_view_archived());
    c.update(ctrl("n"));
    assert_eq!(c.screen(), Screen::Products);
    assert_eq!(c.list.notice.as_ref().unwrap().code, "read_only");
    // Archived products cannot be listed without the permission.
    c.update(toggle("list", 0, "archived", true));
    assert!(!c.list.archived);
    // The form opens read-only: edits are ignored, save explains why.
    c.update(pick("open", 0, 0));
    c.update(Msg::Product(Ok(product(&pid(0), 1))));
    c.update(edit("form", 0, 0, "name", "X"));
    assert_eq!(c.form.as_ref().unwrap().form.data.name, "Teh");
    assert!(c.update(ctrl("s")).effects.is_empty());
    assert_eq!(notice_code(&c), "read_only");
    c.update(press("A", true, true));
    assert_eq!(c.confirm, None, "no archive without the permission");
    // Category and brand writes are refused too.
    c.update(press(&f(3), false, false));
    c.update(edit("category", 0, 0, "name", "Food"));
    assert!(c.update(ctrl("s")).effects.is_empty());
    assert_eq!(
        c.categories.notice.as_ref().unwrap().code,
        "permission_denied_catalog"
    );
    c.update(press(&f(4), false, false));
    c.update(edit("brand", 0, 0, "name", "Aqua"));
    assert!(c.update(ctrl("s")).effects.is_empty());
    // Delete is refused before asking.
    c.update(action("nav-products"));
    c.update(press("D", true, true));
    assert_eq!(c.confirm, None);
    assert_eq!(
        c.list.notice.as_ref().unwrap().code,
        "permission_denied_catalog"
    );
}

#[test]
fn delete_a_product_after_confirming() {
    let mut c = owner();
    c.list.cursor = 1;
    // Ctrl+Shift+D asks, naming the highlighted product; Esc keeps it.
    assert!(c.update(press("D", true, true)).effects.is_empty());
    assert_eq!(c.confirm, Some(Confirm::Delete(Deletable::Product)));
    assert_eq!(c.deleting, Some((pid(1), "Produk 1".into())));
    c.update(press(ESC, false, false));
    assert_eq!((c.confirm, c.deleting.clone()), (None, None));
    // "No" keeps it too; Enter deletes.
    c.update(action("delete-product"));
    c.update(action("confirm-no"));
    assert_eq!(c.deleting, None);
    c.update(action("delete-product"));
    let effects = c.update(press(ENTER, false, false)).effects;
    assert_eq!(
        effects,
        [Effect::Delete {
            token: "t".into(),
            what: Deletable::Product,
            id: pid(1),
        }]
    );
    assert_eq!(c.confirm, None);
    // Done: a notice and the list reloads.
    let effects = c.update(Msg::Deleted(Deletable::Product, Ok(()))).effects;
    assert!(matches!(
        &effects[..],
        [Effect::LoadProducts { append: false, .. }]
    ));
    assert_eq!(c.list.notice.as_ref().unwrap().code, "deleted_done");
    assert_eq!(c.list.cursor, 0);
    // A used product: `in_use` with the reason, nothing reloads.
    let used = ClientError::Api(Problem::new(409, "in_use", "x").with("reason", "used"));
    assert!(
        c.update(Msg::Deleted(Deletable::Product, Err(used)))
            .effects
            .is_empty()
    );
    let n = c.list.notice.clone().unwrap();
    assert_eq!((n.code, n.arg.as_str()), ("in_use", "used"));
    // Without rows there is nothing to delete.
    c.update(Msg::Products(false, Ok(ProductPage::default())));
    c.update(action("delete-product"));
    assert_eq!(c.confirm, None);
    // A login that ended while deleting goes back to login.
    c.update(Msg::Deleted(
        Deletable::Product,
        Err(api(401, "invalid_session")),
    ));
    assert_eq!(c.screen(), Screen::Login);
    assert!(
        c.update(Msg::Deleted(Deletable::Brand, Ok(())))
            .effects
            .is_empty()
    );
}

#[test]
fn delete_categories_and_brands() {
    let mut c = owner();
    let leaf = |id: &str, name: &str| CategoryNode {
        id: id.into(),
        parent_id: None,
        name: name.into(),
        code: String::new(),
        archived: false,
        children: Vec::new(),
    };
    c.update(Msg::Categories(Ok(vec![
        leaf("food", "Food"),
        leaf("typo", "Fod"),
    ])));
    c.update(press(&f(3), false, false));
    c.update(pick("category", 1, 0));
    c.update(press("D", true, true));
    assert_eq!(c.confirm, Some(Confirm::Delete(Deletable::Category)));
    assert_eq!(c.deleting, Some(("typo".into(), "Fod".into())));
    let effects = c.update(action("confirm-yes")).effects;
    assert_eq!(
        effects,
        [Effect::Delete {
            token: "t".into(),
            what: Deletable::Category,
            id: "typo".into(),
        }]
    );
    let effects = c.update(Msg::Deleted(Deletable::Category, Ok(()))).effects;
    assert_eq!(effects.len(), 2, "the taxonomy reloads");
    assert!(c.categories.editing.is_none());
    assert_eq!(c.categories.cursor, 0);
    assert_eq!(c.categories.notice.as_ref().unwrap().code, "deleted_done");
    let children =
        ClientError::Api(Problem::new(409, "in_use", "x").with("reason", "has_children"));
    c.update(Msg::Deleted(Deletable::Category, Err(children)));
    let n = c.categories.notice.clone().unwrap();
    assert_eq!((n.code, n.arg.as_str()), ("in_use", "has_children"));
    // A new (unsaved) category has nothing to delete.
    c.update(action("cat-add-root"));
    c.update(action("cat-delete"));
    assert_eq!(c.confirm, None);

    c.update(Msg::Brands(Ok(vec![Brand {
        id: "a".into(),
        name: "Aqua".into(),
        archived: false,
    }])));
    c.update(press(&f(4), false, false));
    c.update(press("D", true, true));
    assert_eq!(c.deleting, Some(("a".into(), "Aqua".into())));
    let effects = c.update(press("y", false, false)).effects;
    assert!(matches!(
        &effects[..],
        [Effect::Delete { what: Deletable::Brand, id, .. }] if id == "a"
    ));
    let used = ClientError::Api(Problem::new(409, "in_use", "x").with("reason", "has_products"));
    assert!(
        c.update(Msg::Deleted(Deletable::Brand, Err(used)))
            .effects
            .is_empty()
    );
    assert_eq!(c.brands.notice.as_ref().unwrap().code, "in_use");
    assert_eq!(c.brands.editing.as_deref(), Some("a"), "kept");
    assert_eq!(
        c.update(Msg::Deleted(Deletable::Brand, Ok(())))
            .effects
            .len(),
        2
    );
    assert!(c.brands.editing.is_none());
    assert_eq!(c.brands.notice.as_ref().unwrap().code, "deleted_done");
    assert_eq!(notice_for(&api(409, "in_use")).code, "in_use");
}

#[test]
fn list_pages_filters_and_search() {
    let mut c = owner();
    assert_eq!(c.focus, Some("list"));
    // ↓ moves, past the last row it loads the next page.
    c.update(press(DOWN, false, false));
    c.update(press(DOWN, false, false));
    assert_eq!(c.list.cursor, 2);
    let effects = c.update(press(DOWN, false, false)).effects;
    let [Effect::LoadProducts { query, append, .. }] = &effects[..] else {
        panic!("{effects:?}");
    };
    assert!(*append);
    assert_eq!(query.before_id, Some(pid(2)));
    assert!(
        c.update(action("load-more")).effects.is_empty(),
        "one at a time"
    );
    c.update(Msg::Products(
        true,
        Ok(ProductPage {
            items: vec![summary(3)],
            next_before_id: None,
        }),
    ));
    assert_eq!(c.list.rows.len(), 4);
    assert!(
        c.update(action("load-more")).effects.is_empty(),
        "last page"
    );
    c.update(press(UP, false, false));
    assert_eq!(c.list.cursor, 1);
    c.update(pick("products", 3, 0));
    assert_eq!(c.list.cursor, 3);

    // Search: debounced, only the latest keystroke loads.
    let first = c.update(edit("list", 0, 0, "search", "te")).effects;
    let second = c.update(edit("list", 0, 0, "search", " teh ")).effects;
    assert_eq!(first, [Effect::Debounce(1)]);
    assert_eq!(second, [Effect::Debounce(2)]);
    assert!(c.update(Msg::SearchDue(1)).effects.is_empty());
    let effects = c.update(Msg::SearchDue(2)).effects;
    let [Effect::LoadProducts { query, .. }] = &effects[..] else {
        panic!();
    };
    assert_eq!(query.q.as_deref(), Some("teh"));

    // Filters reload the first page.
    c.reference.categories = vec![CategoryRow {
        category: Category {
            id: "c1".into(),
            parent_id: None,
            name: "Minuman".into(),
            code: String::new(),
            archived: false,
        },
        depth: 0,
    }];
    c.reference.brands = vec![Brand {
        id: "b1".into(),
        name: "Aqua".into(),
        archived: false,
    }];
    let effects = c.update(choose("list", 0, "category", 1)).effects;
    assert!(
        matches!(&effects[..], [Effect::LoadProducts { query, .. }] if query.category_id.as_deref() == Some("c1"))
    );
    c.update(choose("list", 0, "brand", 1));
    c.update(choose("list", 0, "type", 2));
    assert_eq!(c.list.brand.as_deref(), Some("b1"));
    assert_eq!(c.list.product_type, Some(ProductType::Weighed));
    c.update(choose("list", 0, "type", 0));
    assert_eq!(c.list.product_type, None);
    let effects = c.update(toggle("list", 0, "archived", true)).effects;
    assert!(matches!(&effects[..], [Effect::LoadProducts { query, .. }] if query.archived));
    assert_eq!(c.list.query(None).limit, Some(PAGE_SIZE));

    // Keys: `/` (unused by any widget) and Ctrl+F focus the search; F5 refreshes; Esc goes back
    // to the list.
    let serial = c.focus_serial;
    assert!(c.update(Msg::Key(Press::parse("/", false, false))).handled);
    assert_eq!((c.focus, c.focus_serial), (Some("search"), serial + 1));
    c.update(press(ESC, false, false));
    assert_eq!(c.focus, Some("list"));
    c.update(ctrl("f"));
    assert_eq!(c.focus, Some("search"));
    assert!(!c.update(Msg::Key(Press::parse("x", false, false))).handled);
    assert_eq!(c.update(press(&f(5), false, false)).effects.len(), 1);
    assert_eq!(c.update(action("refresh")).effects.len(), 1);
    assert!(
        !c.update(press("x", false, false)).handled,
        "typing goes to the field"
    );

    // Errors show on the list; an ended session goes back to login.
    c.update(Msg::Products(false, Err(ClientError::Network("x".into()))));
    assert_eq!(c.list.notice.as_ref().unwrap().code, "network");
    c.update(Msg::Products(false, Err(api(401, "invalid_session"))));
    assert_eq!(c.screen(), Screen::Login);
    assert_eq!(c.login.notice.as_ref().unwrap().code, "session_ended");
    assert_eq!(c.login.username, "owner");
}

#[test]
fn open_edit_and_save_with_blocks_and_image() {
    let mut c = owner();
    // Enter opens the highlighted row.
    let effects = c.update(press(ENTER, false, false)).effects;
    assert!(matches!(&effects[..], [Effect::LoadProduct { id, .. }] if *id == pid(0)));
    let effects = c.update(Msg::Product(Ok(product(&pid(0), 5)))).effects;
    assert!(effects.contains(&Effect::LoadBlocks {
        token: "t".into(),
        id: pid(0)
    }));
    assert_eq!(c.focus, Some("name"));
    c.update(Msg::Blocks(pid(0), Ok(vec![])));
    c.update(Msg::Blocks("other".into(), Ok(vec!["x".into()])));
    c.update(Msg::Image(pid(0), Ok(Some(vec![1, 2]))));
    let form = c.form.as_ref().unwrap();
    assert!(form.form.has_image && form.image.is_some() && !form.form.is_dirty());
    assert!(
        form.form.data.blocked.is_empty(),
        "blocks of another product are ignored"
    );
    c.update(Msg::Component(Ok(product("other", 1))));

    // Edit: name, a flag, the branch checklist; pick an image file.
    c.update(edit("form", 0, 0, "name", "Teh Manis"));
    c.update(toggle("form", 0, "open_price", true));
    c.update(toggle("form", 0, "requires_note", true));
    c.update(toggle("form", 0, "sellable", true));
    c.update(toggle("form", 0, "purchasable", true));
    c.update(toggle("branch", 0, "sold", false));
    c.update(edit("form", 0, 0, "image_path", " /tmp/x.png "));
    assert_eq!(
        c.update(action("pick-image")).effects,
        [Effect::ReadFile("/tmp/x.png".into())]
    );
    c.update(Msg::FileRead(Err("missing".into())));
    assert_eq!(notice_code(&c), "file_error");
    c.update(Msg::FileRead(Ok(vec![9, 9])));
    assert!(c.form.as_ref().unwrap().form.is_dirty());

    // Save: the document first, then the blocks and the image.
    let effects = c.update(ctrl("s")).effects;
    let [Effect::SaveProduct { body, .. }] = &effects[..] else {
        panic!("{effects:?}");
    };
    assert_eq!(body.name, "Teh Manis");
    assert!(body.open_price && body.requires_note);
    assert!(c.update(ctrl("s")).effects.is_empty(), "already saving");
    let mut saved = product(&pid(0), 9);
    saved.name = "Teh Manis".into();
    let effects = c.update(Msg::Saved(Ok(saved))).effects;
    assert_eq!(effects.len(), 2, "{effects:?}");
    assert!(matches!(&effects[0], Effect::PutBlocks { branch_ids, .. } if branch_ids == &["b1"]));
    assert!(matches!(&effects[1], Effect::PutImage { bytes, .. } if bytes == &[9, 9]));
    assert!(c.form.as_ref().unwrap().saving);
    c.update(Msg::BlocksSaved(Ok(vec!["b1".into()])));
    c.update(Msg::ImageSaved(Ok(ImageInfo::default())));
    let form = c.form.as_ref().unwrap();
    assert!(!form.saving && !form.form.is_dirty());
    assert_eq!(notice_code(&c), "saved");
    assert_eq!(c.form.as_ref().unwrap().form.updated_at, 9);

    // Removing the image is applied on the next save; a failed delete is shown.
    c.update(action("remove-image"));
    let effects = c.update(ctrl("s")).effects;
    assert_eq!(effects.len(), 1);
    let effects = c.update(Msg::Saved(Ok(product(&pid(0), 10)))).effects;
    assert!(matches!(&effects[..], [Effect::DeleteImage { .. }]));
    c.update(Msg::ImageDeleted(Err(api(403, "permission_denied"))));
    assert_eq!(notice_code(&c), "permission_denied_catalog");
    // Nothing else to send: saved at once.
    c.update(edit("form", 0, 0, "tags", "a"));
    c.update(ctrl("s"));
    assert!(
        c.update(Msg::Saved(Ok(product(&pid(0), 11))))
            .effects
            .is_empty()
    );
    assert_eq!(notice_code(&c), "saved");
    c.update(Msg::ImageDeleted(Ok(())));
    assert!(!c.form.as_ref().unwrap().form.has_image);
}

#[test]
fn save_errors_go_to_fields_and_tabs() {
    let mut c = with_form();
    // A client-side rule: the core check fails before any call.
    c.update(edit("form", 0, 0, "name", ""));
    assert!(c.update(ctrl("s")).effects.is_empty());
    assert_eq!(notice_code(&c), "fix_errors");
    assert_eq!(c.form.as_ref().unwrap().form.errors[0].field, "name");
    // The server's duplicate: on the barcode row, the tab switches.
    c.update(edit("form", 0, 0, "name", "Teh"));
    c.update(ctrl("3"));
    c.update(edit("form", 0, 0, "new_barcode", "899"));
    c.update(action("add-barcode"));
    c.update(ctrl("1"));
    assert_eq!(c.form.as_ref().unwrap().form.tab, Tab::General);
    assert_eq!(c.update(ctrl("s")).effects.len(), 1);
    let dup = Problem::new(409, "duplicate_barcode", "taken")
        .with("product_id", "x")
        .with("value", "899")
        .with("field", "barcodes.code");
    c.update(Msg::Saved(Err(dup.into())));
    let form = c.form.as_ref().unwrap();
    assert!(!form.saving);
    assert_eq!(form.form.tab, Tab::Units);
    assert_eq!(
        form.form.error_for("barcodes", Some(0)).unwrap().code,
        "duplicate_barcode"
    );
    // Other failures are a notice.
    c.update(ctrl("s"));
    c.update(Msg::Saved(Err(api(403, "read_only_replica"))));
    assert_eq!(notice_code(&c), "read_only_replica");
    // An ended session goes back to login.
    c.update(ctrl("s"));
    c.update(Msg::Saved(Err(api(401, "invalid_session"))));
    assert_eq!(c.screen(), Screen::Login);
}

#[test]
fn escape_asks_before_dropping_changes() {
    let mut c = with_form();
    // Clean: Esc goes straight back.
    c.update(press(ESC, false, false));
    assert_eq!(c.screen(), Screen::Products);
    assert!(c.form.is_none());
    c.update(pick("open", 0, 0));
    c.update(Msg::Product(Ok(product(&pid(0), 1))));
    c.update(edit("form", 0, 0, "name", "Changed"));
    // Dirty: Esc asks; Esc again (no) stays; Enter discards.
    c.update(press(ESC, false, false));
    assert_eq!(c.confirm, Some(Confirm::Discard(Screen::Products)));
    assert!(c.update(ctrl("s")).handled, "the dialog swallows keys");
    assert!(c.update(press(ESC, false, false)).handled);
    assert_eq!((c.confirm, c.screen()), (None, Screen::Form));
    c.update(press(&f(3), false, false));
    assert_eq!(c.confirm, Some(Confirm::Discard(Screen::Categories)));
    c.update(action("confirm-no"));
    c.update(action("nav-brands"));
    c.update(action("anything"));
    assert!(c.confirm.is_some(), "only yes/no while asking");
    c.update(press("y", false, false));
    assert_eq!(c.screen(), Screen::Brands);
    assert!(c.form.is_none());
}

#[test]
fn tabs_archive_and_generate() {
    let mut c = with_form();
    for (key, tab) in [("2", Tab::Variants), ("3", Tab::Units), ("1", Tab::General)] {
        c.update(ctrl(key));
        assert_eq!(c.form.as_ref().unwrap().form.tab, tab);
        assert_eq!(c.focus, Some("list"), "the focus leaves the old tab");
    }
    c.update(action("tab-9"));
    // Archive asks, then archives.
    c.update(press("A", true, true));
    assert_eq!(c.confirm, Some(Confirm::Archive));
    let effects = c.update(press(ENTER, false, false)).effects;
    assert!(matches!(
        &effects[..],
        [Effect::SetArchived { archived: true, .. }]
    ));
    let mut archived = product(&pid(1), 7);
    archived.archived = true;
    c.update(Msg::Archived(Ok(archived)));
    assert!(c.form.as_ref().unwrap().form.archived);
    assert_eq!(notice_code(&c), "archived_done");
    c.update(press("A", true, true));
    c.update(action("confirm-yes"));
    let dup = Problem::new(409, "duplicate_sku", "x")
        .with("product_id", "o")
        .with("value", "TEH");
    c.update(Msg::Archived(Err(dup.into())));
    assert_eq!(
        c.form.as_ref().unwrap().form.errors[0].code,
        "duplicate_sku"
    );
    c.update(Msg::Archived(Ok(product(&pid(1), 8))));
    assert_eq!(notice_code(&c), "unarchived_done");

    // Generate a code: the field shows it (the form is rebuilt).
    let serial = c.form_serial;
    let effects = c.update(action("generate-code")).effects;
    assert!(matches!(
        &effects[..],
        [Effect::Generate {
            what: Generate::ProductCode,
            ..
        }]
    ));
    assert!(
        c.update(action("generate-code")).effects.is_empty(),
        "one at a time"
    );
    c.update(Msg::Generated(Ok("P000042".into())));
    assert_eq!(c.form.as_ref().unwrap().form.data.code, "P000042");
    assert!(c.form_serial > serial);
    // An internal barcode for the picked variant + unit.
    c.update(action("generate-barcode"));
    c.update(Msg::Generated(Ok("2900000000017".into())));
    assert_eq!(
        c.form.as_ref().unwrap().form.data.barcodes[0].code,
        "2900000000017"
    );
    c.update(action("generate-barcode"));
    c.update(Msg::Generated(Err(api(403, "permission_denied"))));
    assert_eq!(notice_code(&c), "permission_denied_catalog");
    // An empty typed barcode adds nothing.
    c.update(action("add-barcode"));
    assert_eq!(c.form.as_ref().unwrap().form.data.barcodes.len(), 1);
    c.update(edit("barcode", 0, 0, "code", "123"));
    c.update(pick("remove-barcode", 0, 0));
    assert!(c.form.as_ref().unwrap().form.data.barcodes.is_empty());
}

#[test]
fn rows_of_the_form() {
    let mut c = with_form();
    // Options, values, combinations and units through the row messages.
    c.update(action("add-option"));
    c.update(edit("option", 0, 0, "name", "Size"));
    c.update(edit("value", 0, 0, "name", "Small"));
    c.update(edit("value", 0, 0, "code", "s"));
    c.update(pick("add-value", 0, 0));
    c.update(edit("value", 0, 1, "name", "Large"));
    c.update(edit("value", 0, 1, "code", "l"));
    c.update(edit("combo", 1, 0, "sku", "big"));
    c.update(edit("combo", 1, 0, "plu", "12"));
    c.update(toggle("combo", 0, "on", false));
    c.update(action("add-unit"));
    c.update(edit("unit", 1, 0, "name", "box"));
    c.update(edit("unit", 1, 0, "factor", "6"));
    c.update(edit("unit", 1, 0, "short_name", "bx"));
    c.update(edit("unit", 0, 0, "factor", "9"));
    for field in ["sellable", "purchasable"] {
        c.update(toggle("unit", 1, field, false));
    }
    c.update(choose("form", 0, "default_purchase", 1));
    c.update(choose("form", 0, "default_sale", 0));
    c.update(choose("form", 0, "pick_unit", 1));
    c.update(choose("form", 0, "pick_variant", 0));
    c.update(choose("form", 0, "base_kind", 1));
    c.update(choose("form", 0, "tax_class", 0));
    c.update(choose("form", 0, "category", 0));
    c.update(choose("form", 0, "brand", 0));
    let form = &c.form.as_ref().unwrap().form;
    assert_eq!(form.data.combos.len(), 2);
    assert!(!form.data.combos[0].on);
    assert_eq!(form.effective_sku(1), "BIG");
    assert_eq!(form.data.units[1].factor, "6");
    assert_eq!(form.data.units[0].factor, "1", "the base factor is fixed");
    assert!(!form.data.units[1].sellable && !form.data.units[1].purchasable);
    assert_eq!(form.data.default_purchase, form.data.units[1].id);
    assert_eq!(form.data.base_kind, sultan_core::catalog::UnitKind::Decimal);
    c.update(pick("remove-unit", 1, 0));
    c.update(pick("remove-value", 0, 1));
    c.update(pick("remove-option", 0, 0));
    let form = &c.form.as_ref().unwrap().form;
    assert!(form.data.options.is_empty() && form.data.units.len() == 1);

    // Bundle components: search, pick a hit (loads its units), set qty and unit, remove.
    c.update(choose("form", 0, "type", 3));
    let effects = c
        .update(edit("form", 0, 0, "component_search", "kaos"))
        .effects;
    assert!(matches!(&effects[..], [Effect::Search { q, .. }] if q == "kaos"));
    let hit = |product: &str, kind: &str| SearchHit {
        product_id: product.into(),
        variant_id: new_id(),
        unit_id: "u1".into(),
        name: "Kaos – S".into(),
        sku: "K-S".into(),
        product_type: kind.into(),
    };
    c.update(Msg::Hits(Ok(vec![
        hit(&pid(1), "stock"),
        hit("pk", "bundle"),
        hit("k", "stock"),
    ])));
    assert_eq!(
        c.form.as_ref().unwrap().hits.len(),
        1,
        "not itself, no bundles"
    );
    let effects = c.update(pick("hit", 0, 0)).effects;
    assert!(matches!(&effects[..], [Effect::LoadComponent { id, .. }] if id == "k"));
    let mut component = product("k", 1);
    component.units.push(sultan_client::catalog::Unit {
        id: "u2".into(),
        name: "box".into(),
        short_name: String::new(),
        kind: "integer".into(),
        factor_thousandths: 6000,
        sellable: true,
        purchasable: true,
        archived: false,
    });
    c.update(Msg::Component(Ok(component)));
    c.update(edit("component", 0, 0, "qty", "2"));
    c.update(choose("component", 0, "unit", 1));
    let line = &c.form.as_ref().unwrap().form.data.components[0];
    assert_eq!((line.qty.as_str(), line.unit_id.as_str()), ("2", "u2"));
    assert!(c.update(pick("hit", 5, 0)).effects.is_empty());
    assert!(
        c.update(edit("form", 0, 0, "component_search", " "))
            .effects
            .is_empty()
    );
    c.update(pick("remove-component", 0, 0));
    assert!(c.form.as_ref().unwrap().form.data.components.is_empty());
    assert!(c.update(pick("nothing", 0, 0)).effects.is_empty());
}

#[test]
fn new_products_start_from_the_filters() {
    let mut c = owner();
    c.list.category = Some("c1".into());
    c.update(ctrl("n"));
    let form = c.form.as_ref().unwrap();
    assert!(form.form.is_new && !form.form.is_dirty());
    assert_eq!(form.form.data.category_id.as_deref(), Some("c1"));
    assert_eq!(form.form.data.tax_class_id, TAX);
    assert_eq!(c.focus, Some("name"));
    // A new product cannot be archived or reloaded.
    c.update(press("A", true, true));
    assert_eq!(c.confirm, None);
    assert!(c.update(action("reload")).effects.is_empty());
}

#[test]
fn events_refresh_and_warn() {
    let mut c = owner();
    let event = |scope: &str, id: &str| ServerEvent {
        kind: "catalog.changed".into(),
        data: json!({ "scope": scope, "product_id": id }),
    };
    // On the list: reload. Old logins' events are ignored.
    assert_eq!(
        c.update(Msg::Event(1, event("product", "x"))).effects.len(),
        1
    );
    assert!(
        c.update(Msg::Event(0, event("product", "x")))
            .effects
            .is_empty()
    );
    assert_eq!(c.update(Msg::Event(1, event("brand", ""))).effects.len(), 2);
    assert!(
        c.update(Msg::Event(1, event("settings", "")))
            .effects
            .is_empty()
    );
    let other = ServerEvent {
        kind: "user.changed".into(),
        data: json!({}),
    };
    assert!(c.update(Msg::Event(1, other)).effects.is_empty());

    // On the form: fetch the open product; newer → warning; reload.
    c.update(pick("open", 1, 0));
    c.update(Msg::Product(Ok(product(&pid(1), 5))));
    assert!(
        c.update(Msg::Event(1, event("product", "zz")))
            .effects
            .is_empty()
    );
    let effects = c.update(Msg::Event(1, event("product", &pid(1)))).effects;
    assert!(matches!(&effects[..], [Effect::CheckProduct { id, .. }] if *id == pid(1)));
    c.update(Msg::Checked(Ok(product(&pid(1), 5))));
    assert!(!c.form.as_ref().unwrap().changed_elsewhere, "same version");
    c.update(Msg::Checked(Ok(product(&pid(1), 6))));
    assert!(c.form.as_ref().unwrap().changed_elsewhere);
    let effects = c.update(action("reload")).effects;
    assert!(matches!(&effects[..], [Effect::LoadProduct { .. }]));
    c.update(Msg::Product(Ok(product(&pid(1), 6))));
    assert!(!c.form.as_ref().unwrap().changed_elsewhere);
    c.update(press(&f(3), false, false));
    assert!(
        c.update(Msg::Event(1, event("product", "x")))
            .effects
            .is_empty()
    );

    // Our own session revoked: back to login.
    let revoked = ServerEvent {
        kind: "session.revoked".into(),
        data: json!({ "this_session": true }),
    };
    c.update(Msg::Event(1, revoked));
    assert_eq!(c.screen(), Screen::Login);
    assert!(
        c.update(Msg::Event(2, event("product", "x")))
            .effects
            .is_empty()
    );
}

#[test]
fn logout_returns_to_login() {
    let mut c = owner();
    let effects = c.update(press(&f(12), false, false)).effects;
    assert_eq!(effects, [Effect::Logout("t".into())]);
    assert_eq!(c.screen(), Screen::Login);
    assert!(c.session.is_none());
    c.update(Msg::LoggedOut(Ok(())));
    assert_eq!(c.login.username, "owner");
}

#[test]
fn reference_failures_and_lookups() {
    let mut c = owner();
    c.update(Msg::Brands(Err(ClientError::Network("x".into()))));
    assert_eq!(c.list.notice.as_ref().unwrap().code, "network");
    c.update(Msg::Me(Err(api(500, "internal"))));
    assert_eq!(c.list.notice.as_ref().unwrap().code, "error");
    c.update(Msg::Product(Err(api(404, "not_found"))));
    assert_eq!(c.list.notice.as_ref().unwrap().code, "not_found");
    let r = Reference {
        categories: vec![CategoryRow {
            category: Category {
                id: "c".into(),
                parent_id: None,
                name: "Lama".into(),
                code: String::new(),
                archived: true,
            },
            depth: 0,
        }],
        brands: vec![Brand {
            id: "b".into(),
            name: "Old".into(),
            archived: true,
        }],
        ..Reference::default()
    };
    assert_eq!(
        r.category_choices(None).len(),
        1,
        "archived ones are hidden"
    );
    assert_eq!(
        r.category_choices(Some("c")).len(),
        2,
        "unless the product has it"
    );
    assert_eq!(r.brand_choices(None).len(), 1);
    assert_eq!(r.brand_choices(Some("b")).len(), 2);
    assert_eq!(r.category_name(Some("c")), "Lama");
    assert_eq!(r.brand_name(Some("x")), "");
}

#[test]
fn categories_editor() {
    let mut c = owner();
    let tree = vec![CategoryNode {
        id: "food".into(),
        parent_id: None,
        name: "Food".into(),
        code: "FD".into(),
        archived: false,
        children: vec![CategoryNode {
            id: "snack".into(),
            parent_id: Some("food".into()),
            name: "Snack".into(),
            code: String::new(),
            archived: false,
            children: Vec::new(),
        }],
    }];
    c.update(Msg::Categories(Ok(tree.clone())));
    assert_eq!(c.reference.categories[1].depth, 1);
    c.update(press(&f(3), false, false));
    assert_eq!(c.screen(), Screen::Categories);
    assert_eq!(c.categories.editing.as_deref(), Some("food"));
    assert_eq!(c.focus, Some("category-name"));
    // A category cannot move under itself or its children.
    assert_eq!(c.parent_choices().len(), 1);
    c.update(press(DOWN, false, false));
    assert_eq!(c.categories.name, "Snack");
    assert_eq!(c.parent_choices().len(), 2);
    c.update(press(UP, false, false));
    assert_eq!(c.categories.cursor, 0);
    // Rename and save.
    c.update(edit("category", 0, 0, "name", "Makanan"));
    c.update(edit("category", 0, 0, "code", "mkn"));
    let effects = c.update(ctrl("s")).effects;
    let [Effect::PutCategory { id, body, .. }] = &effects[..] else {
        panic!();
    };
    assert_eq!((id.as_str(), body.name.as_str()), ("food", "Makanan"));
    let saved = Category {
        id: "food".into(),
        parent_id: None,
        name: "Makanan".into(),
        code: "MKN".into(),
        archived: false,
    };
    assert_eq!(c.update(Msg::CategorySaved(Ok(saved))).effects.len(), 2);
    c.update(Msg::Categories(Ok(tree)));
    assert_eq!(c.categories.notice.as_ref().unwrap().code, "saved");
    // Add a child of the selected one: Ctrl+N; an empty name is refused.
    c.update(ctrl("n"));
    assert_eq!(c.categories.parent.as_deref(), Some("food"));
    assert!(c.categories.editing.is_none());
    assert!(c.update(ctrl("s")).effects.is_empty());
    assert_eq!(c.categories.notice.as_ref().unwrap().code, "name_required");
    c.update(edit("category", 0, 0, "name", "Minuman"));
    c.update(choose("category", 0, "parent", 0));
    let effects = c.update(ctrl("s")).effects;
    assert!(matches!(&effects[..], [Effect::PutCategory { body, .. }] if body.parent_id.is_none()));
    c.update(Msg::CategorySaved(Err(api(422, "duplicate_category_name"))));
    assert_eq!(c.categories.notice.as_ref().unwrap().code, "duplicate_name");
    c.update(action("cat-add-root"));
    assert!(c.categories.parent.is_none());
    // Archive the selected one.
    c.update(pick("category", 1, 0));
    let effects = c.update(press("A", true, true)).effects;
    assert!(matches!(&effects[..], [Effect::PutCategory { body, .. }] if body.archived));
    assert_eq!(c.update(press(&f(5), false, false)).effects.len(), 2);
    assert_eq!(c.update(action("refresh")).effects.len(), 2);
    c.update(press(ESC, false, false));
    assert_eq!(c.screen(), Screen::Products);
}

#[test]
fn brands_editor() {
    let mut c = owner();
    c.update(Msg::Brands(Ok(vec![
        Brand {
            id: "a".into(),
            name: "Aqua".into(),
            archived: false,
        },
        Brand {
            id: "s".into(),
            name: "Sosro".into(),
            archived: true,
        },
    ])));
    c.update(action("nav-brands"));
    assert_eq!(c.brands.name, "Aqua");
    c.update(press(DOWN, false, false));
    assert_eq!(c.brands.editing.as_deref(), Some("s"));
    let effects = c.update(press("A", true, true)).effects;
    assert!(matches!(&effects[..], [Effect::PutBrand { body, .. }] if !body.archived));
    c.update(ctrl("n"));
    assert!(c.brands.editing.is_none());
    assert!(c.update(action("brand-save")).effects.is_empty());
    c.update(edit("brand", 0, 0, "name", " Indomie "));
    let effects = c.update(ctrl("s")).effects;
    let [Effect::PutBrand { id, body, .. }] = &effects[..] else {
        panic!();
    };
    assert_eq!(body.name, "Indomie");
    let saved = Brand {
        id: id.clone(),
        name: "Indomie".into(),
        archived: false,
    };
    assert_eq!(
        c.update(Msg::BrandSaved(Ok(saved.clone()))).effects.len(),
        2
    );
    c.update(Msg::Brands(Ok(vec![saved])));
    assert_eq!(c.brands.cursor, 0);
    c.update(Msg::BrandSaved(Err(api(422, "duplicate_brand_name"))));
    assert_eq!(c.brands.notice.as_ref().unwrap().code, "duplicate_name");
    c.update(press(ESC, false, false));
    c.update(action("nav-categories"));
    assert_eq!(c.screen(), Screen::Categories);
    c.update(action("nav-products"));
    assert_eq!(c.screen(), Screen::Products);
}

#[test]
fn a_bundle_loads_its_component_units() {
    let mut c = owner();
    c.update(pick("open", 0, 0));
    let effects = c.update(Msg::Product(Ok(bundle(&pid(0))))).effects;
    assert!(effects.contains(&Effect::LoadComponent {
        token: "t".into(),
        id: "other".into()
    }));
    let line = &c.form.as_ref().unwrap().form.data.components[0];
    assert_eq!((line.name.as_str(), line.units.len()), ("Teh – Manis", 1));
    let mut other = product("other", 1);
    other.units[0].id = pid(99);
    c.update(Msg::Component(Ok(other)));
    assert!(!c.form.as_ref().unwrap().form.is_dirty());
    assert!(
        c.update(ctrl("s")).effects.len() == 1,
        "{:?}",
        c.form.as_ref().unwrap().form.errors
    );
    c.update(Msg::Saved(Ok(bundle(&pid(0)))));
    let line = &c.form.as_ref().unwrap().form.data.components[0];
    assert_eq!(line.units.len(), 1, "unit choices survive the save");
}
