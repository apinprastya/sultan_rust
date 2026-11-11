use sultan_client::{Me, Problem};

use super::*;
use crate::backoffice::{Msg, Session};

fn profile(name: &str) -> Profile {
    Profile {
        name: name.into(),
        ..Profile::default()
    }
}

#[test]
fn profile_body_checks_every_field() {
    let ok = Profile {
        phone: "0812-3456-789".into(),
        email: " Budi@Example.com ".into(),
        birth_day: "29".into(),
        birth_month: "2".into(),
        tax_id: "01.234.567.8-901.000".into(),
        tier: Some("t".into()),
        valid_until: "2027-1-5".into(),
        notes: "  ".into(),
        ..profile(" Budi ")
    };
    let body = ok.body(Some("K7QH29XM")).unwrap();
    assert_eq!(body.name, "Budi");
    assert_eq!(body.code.as_deref(), Some("K7QH29XM"));
    assert_eq!(
        (body.birth_month, body.birth_day, body.birth_year),
        (Some(2), Some(29), None)
    );
    assert_eq!(body.tier_valid_until.as_deref(), Some("2027-01-05"));
    assert_eq!(body.notes, None, "blank text is none");

    let table: Vec<(Profile, Vec<&str>)> = vec![
        (profile(""), vec!["name"]),
        (
            Profile {
                phone: "12".into(),
                email: "nope".into(),
                tax_id: "123".into(),
                ..profile("A")
            },
            vec!["phone", "email", "tax_id"],
        ),
        (
            Profile {
                birth_day: "31".into(),
                birth_month: "2".into(),
                ..profile("A")
            },
            vec!["birthday"],
        ),
        (
            Profile {
                birth_year: "1990".into(),
                ..profile("A")
            },
            vec!["birthday"],
        ),
        (
            Profile {
                birth_day: "1".into(),
                birth_month: "1".into(),
                birth_year: "x".into(),
                ..profile("A")
            },
            vec!["birthday"],
        ),
        (
            Profile {
                valid_until: "2027-12-31".into(),
                ..profile("A")
            },
            vec!["tier_valid_until"],
        ),
        (
            Profile {
                tier: Some("t".into()),
                valid_until: "31/12/2027".into(),
                ..profile("A")
            },
            vec!["tier_valid_until"],
        ),
    ];
    for (p, fields) in table {
        let issues = p.body(None).unwrap_err();
        let got: Vec<&str> = issues.iter().map(|i| i.field).collect();
        assert_eq!(got, fields, "{p:?}");
    }
    let year = Profile {
        birth_day: "1".into(),
        birth_month: "12".into(),
        birth_year: "1990".into(),
        ..profile("A")
    };
    assert_eq!(year.body(None).unwrap().birth_year, Some(1990));
}

#[test]
fn profile_round_trip_and_helpers() {
    let c = Customer {
        id: "c".into(),
        code: "K7QH29XM".into(),
        name: "Budi".into(),
        phone: Some("+628123456789".into()),
        birth_month: Some(2),
        birth_day: Some(29),
        birth_year: Some(1990),
        tier_valid_until: Some("2027-12-31".into()),
        ..Customer::default()
    };
    let p = Profile::of(&c);
    assert_eq!(p.phone, "08123456789");
    assert_eq!(
        (p.birth_day.as_str(), p.birth_year.as_str()),
        ("29", "1990")
    );
    assert_eq!(local_phone("+6591234567"), "+6591234567");
    assert_eq!(local_phone("garbage"), "garbage");
    assert_eq!(amount_text("5.000"), Some(5_000));
    assert_eq!(amount_text(" 100 000 "), Some(100_000));
    assert_eq!(amount_text("-5"), None);
    assert_eq!(amount_text(""), None);
    assert!(in_section(Screen::CustomerForm) && in_section(Screen::BalanceSettings));
    assert!(!in_section(Screen::Products));

    let list = CustomerList {
        search: " budi ".into(),
        archived: true,
        ..CustomerList::default()
    };
    let q = list.query(Some(2));
    assert_eq!(
        (q.q.as_str(), q.include_archived, q.page),
        ("budi", false, Some(2))
    );
    let all = CustomerList {
        archived: true,
        ..CustomerList::default()
    };
    assert!(all.query(None).include_archived);
}

#[test]
fn choices_keep_the_archived_pick() {
    let state = CustomerState {
        groups: vec![
            CustomerGroup {
                id: "g1".into(),
                name: "Grosir".into(),
                archived: false,
            },
            CustomerGroup {
                id: "g2".into(),
                name: "Old".into(),
                archived: true,
            },
        ],
        tiers: vec![MembershipTier {
            id: "t1".into(),
            name: "Gold".into(),
            rank: 2,
            archived: true,
        }],
        ..CustomerState::default()
    };
    assert_eq!(state.group_choices(None).len(), 2);
    assert_eq!(state.group_choices(Some("g2")).len(), 3);
    assert_eq!(state.tier_choices(None).len(), 1);
    assert_eq!(state.tier_choices(Some("t1")).len(), 2);
}

#[test]
fn server_errors_go_to_their_field() {
    let mut f = CustomerForm::new("c".into());
    let cases = [
        (
            Problem::new(422, "invalid_phone", "x"),
            "phone",
            "invalid_phone",
        ),
        (
            Problem::new(422, "invalid_tax_id", "x"),
            "tax_id",
            "invalid_tax_id",
        ),
        (
            Problem::new(422, "invalid_field", "x").with("field", "name"),
            "name",
            "invalid_customer_name",
        ),
        (
            Problem::new(422, "invalid_field", "x").with("field", "email"),
            "email",
            "invalid_email",
        ),
        (
            Problem::new(422, "invalid_field", "x").with("field", "birth_year"),
            "birthday",
            "invalid_birthday",
        ),
        (
            Problem::new(422, "invalid_field", "x").with("field", "tier_valid_until"),
            "tier_valid_until",
            "invalid_date",
        ),
    ];
    for (p, field, code) in cases {
        assert!(f.apply_problem(&p));
        assert_eq!(f.issue(field).map(|i| i.code), Some(code));
    }
    let taken = Problem::new(409, "phone_taken", "x")
        .with("customer_code", "M4RT-2PQA")
        .with("masked_phone", "0812*****432");
    assert!(f.apply_problem(&taken));
    assert_eq!(f.issue("phone").unwrap().arg, "M4RT-2PQA · 0812*****432");
    assert!(!f.apply_problem(&Problem::new(409, "archived", "x")));
}

fn controller(permissions: &[&str]) -> Controller {
    let mut c = Controller::new("owner");
    c.session = Some(Session {
        token: "t".into(),
        me: Me {
            user_id: "u".into(),
            permissions_anywhere: permissions.iter().map(|s| (*s).to_owned()).collect(),
            ..Me::default()
        },
        display_name: "Owner".into(),
    });
    c.screen = Some(Screen::Products);
    c
}

fn reply(c: &mut Controller, r: Reply) -> Vec<Effect> {
    c.update(Msg::Customers(Box::new(r))).effects
}

fn calls(effects: &[Effect]) -> Vec<Call> {
    effects
        .iter()
        .filter_map(|e| match e {
            Effect::Customers { call, .. } => Some(call.clone()),
            _ => None,
        })
        .collect()
}

#[test]
fn failures_and_events_land_on_the_right_screen() {
    let all = [
        CUSTOMER_SEARCH,
        CUSTOMER_MANAGE,
        CUSTOMER_CREATE,
        CUSTOMER_EDIT,
    ];
    let mut c = controller(&all);
    let effects = c.update(Msg::Action("nav-customers".into())).effects;
    assert_eq!(
        calls(&effects),
        [
            Call::Groups(true),
            Call::Tiers(true),
            Call::Search {
                query: c.customers.list.query(None),
                append: false,
            },
        ]
    );
    let network = ClientError::Network("down".into());
    reply(&mut c, Reply::Page(false, Err(network.clone())));
    assert_eq!(c.customers.list.notice.as_ref().unwrap().code, "network");
    reply(&mut c, Reply::Groups(Err(network.clone())));
    reply(&mut c, Reply::Tiers(Err(network.clone())));
    assert!(c.customers.group_editor.notice.is_some());
    reply(&mut c, Reply::Opened(Err(network.clone())));
    assert!(c.customers.list.notice.is_some());

    // An event reloads the list on view, but not while it loads.
    let event = sultan_client::ServerEvent {
        kind: "customer.changed".into(),
        data: serde_json::json!({}),
    };
    let generation = 0;
    let effects = c.update(Msg::Event(generation, event.clone())).effects;
    assert_eq!(calls(&effects).len(), 1);
    let effects = c.update(Msg::Event(generation, event)).effects;
    assert!(effects.is_empty(), "still loading");

    // Settings: a server field error.
    c.update(Msg::Action("tab-balance-settings".into()));
    reply(
        &mut c,
        Reply::SettingsSaved(Err(Problem::new(422, "invalid_field", "x")
            .with("field", "max_balance")
            .into())),
    );
    assert_eq!(c.customers.settings.issues[0].field, "max_balance");
    reply(
        &mut c,
        Reply::SettingsSaved(Err(Problem::new(422, "invalid_field", "x")
            .with("field", "max_saved_per_sale")
            .into())),
    );
    assert_eq!(c.customers.settings.issues.len(), 2);

    // A merge failure stays on the merge panel; a session that ended logs out.
    reply(&mut c, Reply::Merged(Err(network)));
    assert_eq!(c.customers.merge.notice.as_ref().unwrap().code, "network");
    reply(
        &mut c,
        Reply::Partner(Err(Problem::new(401, "invalid_session", "x").into())),
    );
    assert_eq!(c.screen(), Screen::Login);
}

#[test]
fn form_writes_without_a_form_do_nothing() {
    let mut c = controller(&[CUSTOMER_MANAGE, CUSTOMER_ERASE]);
    let ok = Customer {
        id: "c".into(),
        ..Customer::default()
    };
    for r in [
        Reply::Saved(Ok(ok.clone())),
        Reply::Archived(Ok(ok.clone())),
        Reply::Erased(Ok(ok.clone())),
    ] {
        assert!(reply(&mut c, r).is_empty());
    }
    assert!(c.customers.form.is_none());
    assert!(c.customer_confirmed(Confirm::CustomerArchive).is_empty());
    assert!(c.customer_confirmed(Confirm::Merge).is_empty());
    assert!(c.customer_confirmed(Confirm::Archive).is_empty());
    assert!(!c.erase_code_matches());
    assert!(!c.customer_editable());
    // Unknown fields and kinds are ignored.
    assert!(c.customer_edit("customer", "nope", "x".into()).is_empty());
    assert!(c.customer_choose("other", "tier", 0).is_none());
    assert!(c.customer_toggle("other", "x", true).is_none());
    assert!(c.customer_pick("other", 0).is_none());
    assert_eq!(c.customer_pick("customer-open", 5), Some(Vec::new()));
    assert_eq!(c.customer_pick("negative-open", 5), Some(Vec::new()));
    assert!(c.customer_action("nope").is_none());
}

#[test]
fn group_tier_and_settings_screens_without_manage() {
    let mut c = controller(&[CUSTOMER_SEARCH]);
    c.update(Msg::Action("tab-groups".into()));
    c.update(Msg::Action("group-save".into()));
    assert_eq!(
        c.customers.group_editor.notice.as_ref().unwrap().arg,
        CUSTOMER_MANAGE
    );
    c.update(Msg::Action("tier-add".into()));
    assert_eq!(c.customers.tier_editor.rank, "1");
    c.update(Msg::Action("merge".into()));
    assert!(c.customers.merge.notice.is_some());
    c.update(Msg::Action("merge-swap".into()));
    assert!(c.customers.merge.from.is_none());
    // Without the manage permission the archived toggle does nothing.
    assert_eq!(
        c.customer_toggle("customer-list", "archived", true),
        Some(Vec::new())
    );
    assert!(!c.customers.list.archived);
    // An empty name is refused before the call.
    let mut m = controller(&[CUSTOMER_MANAGE]);
    m.update(Msg::Action("tab-groups".into()));
    m.update(Msg::Action("group-add".into()));
    assert!(
        m.update(Msg::Action("group-save".into()))
            .effects
            .is_empty()
    );
    assert_eq!(
        m.customers.group_editor.notice.as_ref().unwrap().code,
        "name_required"
    );
    // Archive of a new (unsaved) group does nothing.
    assert!(
        m.update(Msg::Action("group-archive".into()))
            .effects
            .is_empty()
    );
}
