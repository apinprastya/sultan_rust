//! Pairing codes, device tokens, auth sessions, login attempts and override tokens.

use sultan_core::access::{LoginAttempts, Permission, Requested};
use sultan_core::ids::RegisterId;
use sultan_core::money::Money;
use sultan_db::auth::{self, AuthSession};
use sultan_db::devices::{self, DeviceIdentity};
use sultan_db::overrides::{self, Approval, OverrideToken, OverrideUse};
use sultan_db::{Db, DbError, org};

use crate::common::{db_tests, hash, org, t};

db_tests!(
    pairing_code_is_single_use,
    pairing_code_expires,
    issuing_a_new_pairing_code_expires_the_old_one,
    device_token_lookup_and_revoke,
    auth_session_lifecycle,
    login_attempts_round_trip,
    override_token_is_single_use,
    override_token_must_match,
);

/// Spec 2.1 §9 case 11.
async fn pairing_code_is_single_use(db: &Db) {
    let o = org(db).await;
    devices::create_pairing_code(db, &hash(1), o.register.id, t(0), t(600_000))
        .await
        .unwrap();
    assert_eq!(
        devices::consume_pairing_code(db, &hash(1), t(1_000))
            .await
            .unwrap(),
        Some(o.register.id)
    );
    assert_eq!(
        devices::consume_pairing_code(db, &hash(1), t(2_000))
            .await
            .unwrap(),
        None,
        "reused code"
    );
    assert_eq!(
        devices::consume_pairing_code(db, &hash(9), t(2_000))
            .await
            .unwrap(),
        None,
        "unknown code"
    );
    let err = devices::create_pairing_code(db, &hash(2), RegisterId::generate(), t(0), t(1))
        .await
        .unwrap_err();
    assert!(matches!(err, DbError::MissingReference(_)), "{err}");
}

async fn pairing_code_expires(db: &Db) {
    let o = org(db).await;
    devices::create_pairing_code(db, &hash(1), o.register.id, t(0), t(600_000))
        .await
        .unwrap();
    assert_eq!(
        devices::consume_pairing_code(db, &hash(1), t(600_000))
            .await
            .unwrap(),
        None,
        "expired exactly at expires_at"
    );
}

async fn issuing_a_new_pairing_code_expires_the_old_one(db: &Db) {
    let o = org(db).await;
    for code in [1, 2] {
        devices::create_pairing_code(db, &hash(code), o.register.id, t(0), t(600_000))
            .await
            .unwrap();
    }
    // Code 2 is used up; only code 1 is still usable and gets expired.
    devices::consume_pairing_code(db, &hash(2), t(1))
        .await
        .unwrap();
    assert_eq!(
        devices::expire_pairing_codes(db, o.register.id, t(10))
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        devices::consume_pairing_code(db, &hash(1), t(11))
            .await
            .unwrap(),
        None
    );
    assert_eq!(
        devices::expire_pairing_codes(db, o.register.id, t(12))
            .await
            .unwrap(),
        0
    );
}

async fn device_token_lookup_and_revoke(db: &Db) {
    let o = org(db).await;
    let identity = DeviceIdentity {
        register: o.register.id,
        branch: o.jkt.id,
    };
    devices::insert_device_token(db, &hash(1), o.register.id, t(0))
        .await
        .unwrap();
    devices::insert_device_token(db, &hash(2), o.register.id, t(0))
        .await
        .unwrap();
    assert_eq!(
        devices::find_device(db, &hash(1)).await.unwrap(),
        Some(identity)
    );
    assert_eq!(devices::find_device(db, &hash(7)).await.unwrap(), None);

    assert!(
        devices::revoke_device_token(db, &hash(1), t(5))
            .await
            .unwrap()
    );
    assert!(
        !devices::revoke_device_token(db, &hash(1), t(6))
            .await
            .unwrap()
    );
    assert_eq!(devices::find_device(db, &hash(1)).await.unwrap(), None);
    assert_eq!(
        devices::find_device(db, &hash(2)).await.unwrap(),
        Some(identity)
    );

    // An archived register's tokens stop working.
    let mut archived = o.register.clone();
    archived.archived = true;
    let mut tx = db.begin().await.unwrap();
    org::save_register(&mut tx, &archived, t(7)).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(devices::find_device(db, &hash(2)).await.unwrap(), None);

    // Unpair revokes everything left.
    assert_eq!(
        devices::revoke_register_tokens(db, o.register.id, t(8))
            .await
            .unwrap(),
        1
    );
}

async fn auth_session_lifecycle(db: &Db) {
    let o = org(db).await;
    let session = AuthSession {
        token_hash: hash(3),
        user: o.cashier.id,
        register: Some(o.register.id),
        branch: o.jkt.id,
        created_at: t(0),
        last_seen: t(0),
        expires_at: t(1_000),
        revoked_at: None,
    };
    auth::create_session(db, &session).await.unwrap();
    let back_office = AuthSession {
        token_hash: hash(4),
        register: None,
        ..session
    };
    auth::create_session(db, &back_office).await.unwrap();

    assert_eq!(
        auth::find_session(db, &hash(3)).await.unwrap(),
        Some(session)
    );
    assert_eq!(
        auth::find_session(db, &hash(4)).await.unwrap(),
        Some(back_office)
    );
    assert_eq!(auth::find_session(db, &hash(5)).await.unwrap(), None);

    assert!(
        auth::touch_session(db, &hash(3), t(500), t(1_500))
            .await
            .unwrap()
    );
    let touched = auth::find_session(db, &hash(3)).await.unwrap().unwrap();
    assert_eq!((touched.last_seen, touched.expires_at), (t(500), t(1_500)));
    assert!(touched.is_active(t(1_499)));
    assert!(!touched.is_active(t(1_500)));
    assert!(
        !auth::touch_session(db, &hash(3), t(1_500), t(2_500))
            .await
            .unwrap(),
        "expired sessions are not revived"
    );

    assert!(auth::revoke_session(db, &hash(4), t(600)).await.unwrap());
    assert!(!auth::revoke_session(db, &hash(4), t(700)).await.unwrap());
    let revoked = auth::find_session(db, &hash(4)).await.unwrap().unwrap();
    assert_eq!(revoked.revoked_at, Some(t(600)));
    assert!(!revoked.is_active(t(601)));
    assert!(
        !auth::touch_session(db, &hash(4), t(601), t(5_000))
            .await
            .unwrap()
    );

    assert_eq!(
        auth::revoke_user_sessions(db, o.cashier.id, t(800))
            .await
            .unwrap(),
        1
    );
}

async fn login_attempts_round_trip(db: &Db) {
    let o = org(db).await;
    let fresh = auth::load_login_attempts(db, o.cashier.id, o.jkt.id)
        .await
        .unwrap();
    assert_eq!(fresh, LoginAttempts::default());

    let mut attempts = fresh;
    for i in 0..5 {
        let _ = attempts.record_failure(t(i));
    }
    assert!(attempts.is_locked(t(10)));
    auth::save_login_attempts(db, o.cashier.id, o.jkt.id, &attempts)
        .await
        .unwrap();
    let loaded = auth::load_login_attempts(db, o.cashier.id, o.jkt.id)
        .await
        .unwrap();
    assert_eq!(loaded, attempts);
    // Per branch: BDG01 is unaffected.
    assert_eq!(
        auth::load_login_attempts(db, o.cashier.id, o.bdg.id)
            .await
            .unwrap(),
        LoginAttempts::default()
    );

    // Overwrite (the upsert path).
    let mut unlocked = loaded;
    unlocked.unlock();
    unlocked.record_failure(t(20)).unwrap();
    auth::save_login_attempts(db, o.cashier.id, o.jkt.id, &unlocked)
        .await
        .unwrap();
    assert_eq!(
        auth::load_login_attempts(db, o.cashier.id, o.jkt.id)
            .await
            .unwrap(),
        unlocked
    );
}

fn token(o: &crate::common::Org) -> OverrideToken {
    OverrideToken {
        token_hash: hash(8),
        permission: Permission::SaleDiscount,
        value: Some(Requested::Pct(1_500)),
        register: o.register.id,
        context_ref: "cart-1".into(),
        requester: o.cashier.id,
        approver: o.manager.id,
        created_at: t(0),
        expires_at: t(120_000),
    }
}

fn use_of(token: &OverrideToken) -> OverrideUse<'_> {
    OverrideUse {
        token_hash: &token.token_hash,
        permission: token.permission,
        value: token.value,
        register: token.register,
        context_ref: &token.context_ref,
    }
}

/// Spec 2.1 §9 case 15 (reuse).
async fn override_token_is_single_use(db: &Db) {
    let o = org(db).await;
    let tok = token(&o);
    overrides::insert(db, &tok).await.unwrap();
    let approval = Approval {
        requester: o.cashier.id,
        approver: o.manager.id,
    };
    assert_eq!(
        overrides::consume(db, &use_of(&tok), t(1_000))
            .await
            .unwrap(),
        Some(approval)
    );
    assert_eq!(
        overrides::consume(db, &use_of(&tok), t(1_001))
            .await
            .unwrap(),
        None
    );

    // A token without a value (non-limited permission).
    let void = OverrideToken {
        token_hash: hash(9),
        permission: Permission::SaleVoid,
        value: None,
        ..tok.clone()
    };
    overrides::insert(db, &void).await.unwrap();
    assert_eq!(
        overrides::consume(db, &use_of(&void), t(1_000))
            .await
            .unwrap(),
        Some(approval)
    );
}

/// Spec 2.1 §9 case 15 (another value / document / register / action, expired).
async fn override_token_must_match(db: &Db) {
    let o = org(db).await;
    let tok = token(&o);
    overrides::insert(db, &tok).await.unwrap();
    let base = use_of(&tok);
    let other_register = RegisterId::generate();
    let mismatches = [
        OverrideUse {
            value: Some(Requested::Pct(1_600)),
            ..base
        },
        OverrideUse {
            value: None,
            ..base
        },
        OverrideUse {
            context_ref: "cart-2",
            ..base
        },
        OverrideUse {
            register: other_register,
            ..base
        },
        OverrideUse {
            permission: Permission::SaleCartDiscount,
            ..base
        },
        OverrideUse {
            permission: Permission::CashOut,
            value: Some(Requested::Amount(Money::from_minor(1_500))),
            ..base
        },
    ];
    for m in &mismatches {
        assert_eq!(
            overrides::consume(db, m, t(1_000)).await.unwrap(),
            None,
            "{m:?}"
        );
    }
    assert_eq!(
        overrides::consume(db, &base, t(120_000)).await.unwrap(),
        None,
        "expired"
    );
    // Mismatches did not use the token up.
    assert!(
        overrides::consume(db, &base, t(119_999))
            .await
            .unwrap()
            .is_some()
    );
}
