//! `GET /api/v1/openapi.json` (plan Phase 5): a valid `OpenAPI` document listing every route.

use axum::http::{Method, StatusCode};

use crate::common::{Harness, api_tests};

api_tests!(openapi_lists_the_routes);

#[allow(clippy::too_many_lines)] // one table row per route and schema
async fn openapi_lists_the_routes(h: &Harness) {
    // No authentication needed.
    let r = h.send(Method::GET, "/api/v1/openapi.json", &[], None).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.headers["content-type"], "application/json");
    let doc = &r.body;
    assert!(doc["openapi"].as_str().unwrap().starts_with("3."));
    assert_eq!(doc["info"]["title"], "Sultan POS API");
    let paths = doc["paths"].as_object().unwrap();
    #[rustfmt::skip]
    let expected = [
        ("/api/v1/auth/pin", "post"),
        ("/api/v1/auth/password", "post"),
        ("/api/v1/auth/me", "get"),
        ("/api/v1/auth/logout", "post"),
        ("/api/v1/registers/pair", "post"),
        ("/api/v1/registers/me", "get"),
        ("/api/v1/registers/me/users", "get"),
        ("/api/v1/registers", "get"),
        ("/api/v1/registers/{id}", "get"), ("/api/v1/registers/{id}", "put"), ("/api/v1/registers/{id}", "delete"),
        ("/api/v1/registers/{id}/pairing-code", "post"),
        ("/api/v1/registers/{id}/unpair", "post"),
        ("/api/v1/branches", "get"),
        ("/api/v1/branches/{id}", "get"), ("/api/v1/branches/{id}", "put"), ("/api/v1/branches/{id}", "delete"),
        ("/api/v1/locations", "get"),
        ("/api/v1/locations/{id}", "get"), ("/api/v1/locations/{id}", "put"), ("/api/v1/locations/{id}", "delete"),
        ("/api/v1/users", "get"),
        ("/api/v1/users/{id}", "get"), ("/api/v1/users/{id}", "put"), ("/api/v1/users/{id}", "delete"),
        ("/api/v1/users/{id}/unlock", "post"),
        ("/api/v1/roles", "get"),
        ("/api/v1/roles/{id}", "get"), ("/api/v1/roles/{id}", "put"), ("/api/v1/roles/{id}", "delete"),
        ("/api/v1/permissions", "get"),
        ("/api/v1/audit", "get"),
        ("/api/v1/cashier-sessions", "post"), ("/api/v1/cashier-sessions", "get"),
        ("/api/v1/cashier-sessions/current", "get"),
        ("/api/v1/cashier-sessions/{id}", "get"),
        ("/api/v1/cashier-sessions/{id}/close", "post"),
        ("/api/v1/cashier-sessions/{id}/force-close", "post"),
        ("/api/v1/overrides", "post"),
        ("/api/v1/demo/override-protected", "post"),
        ("/api/v1/products", "get"),
        ("/api/v1/products/{id}", "get"), ("/api/v1/products/{id}", "put"),
        ("/api/v1/products/{id}", "delete"),
        ("/api/v1/products/{id}/archive", "post"), ("/api/v1/products/{id}/unarchive", "post"),
        ("/api/v1/products/{id}/branch-blocks", "get"), ("/api/v1/products/{id}/branch-blocks", "put"),
        ("/api/v1/products/{id}/image", "get"), ("/api/v1/products/{id}/image", "put"),
        ("/api/v1/products/{id}/image", "delete"),
        ("/api/v1/categories", "get"),
        ("/api/v1/categories/{id}", "get"), ("/api/v1/categories/{id}", "put"),
        ("/api/v1/categories/{id}", "delete"),
        ("/api/v1/brands", "get"),
        ("/api/v1/brands/{id}", "get"), ("/api/v1/brands/{id}", "put"),
        ("/api/v1/brands/{id}", "delete"),
        ("/api/v1/tax-classes", "get"),
        ("/api/v1/catalog/lookup", "get"),
        ("/api/v1/catalog/search", "get"),
        ("/api/v1/settings/catalog", "get"), ("/api/v1/settings/catalog", "put"),
        ("/api/v1/catalog/generate/product-code", "post"),
        ("/api/v1/catalog/generate/barcode", "post"),
        ("/api/v1/events", "get"),
        ("/api/v1/openapi.json", "get"),
    ];
    for (path, method) in expected {
        assert!(
            paths.get(path).and_then(|p| p.get(method)).is_some(),
            "{method} {path}"
        );
    }
    let operations: usize = paths.values().map(|p| p.as_object().unwrap().len()).sum();
    assert_eq!(operations, expected.len(), "no undocumented extras");

    let components = &doc["components"];
    for schema in [
        "Problem",
        "BranchDto",
        "UserBody",
        "UserDto",
        "RoleDto",
        "AuditPage",
        "SessionDto",
        "CloseReportDto",
        "SessionDetailDto",
        "SessionPage",
        "OverrideResponse",
        "ProductBody",
        "ProductDto",
        "ProductPage",
        "UnitDto",
        "VariantDto",
        "BarcodeDto",
        "ComponentDto",
        "BranchBlocksDto",
        "ImageDto",
        "CategoryTreeDto",
        "BrandDto",
        "LookupDto",
        "SearchResult",
        "CatalogSettingsDto",
        "GeneratedDto",
        "ProblemItem",
        "MeResponse",
        "PairResponse",
        "RegisterMe",
    ] {
        assert!(components["schemas"].get(schema).is_some(), "{schema}");
    }
    for scheme in ["bearer", "register_token"] {
        assert!(
            components["securitySchemes"].get(scheme).is_some(),
            "{scheme}"
        );
    }
    // The problem body documents its optional members, e.g. `limit` and `permission` of
    // `override_limit_exceeded` and the catalog's `errors` list.
    let problem = &components["schemas"]["Problem"]["properties"];
    for member in [
        "permission",
        "limit",
        "reason",
        "field",
        "errors",
        "product_id",
        "variant_id",
        "value",
    ] {
        assert!(problem.get(member).is_some(), "Problem.{member}");
    }
    // Quantities are documented as thousandths.
    assert!(
        components["schemas"]["UnitDto"]["properties"]
            .get("factor_thousandths")
            .is_some()
    );

    // Every $ref points at a defined schema.
    let text = doc.to_string();
    for part in text.split("\"$ref\":\"#/components/schemas/").skip(1) {
        let name = &part[..part.find('"').unwrap()];
        assert!(
            components["schemas"].get(name).is_some(),
            "dangling $ref {name}"
        );
    }
}
