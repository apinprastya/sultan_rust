//! The category tree: moves, cycles, depth and sibling names (spec 2.2 §8).

use std::collections::HashMap;

use super::CatalogError;
use super::code::check_category_code;
use super::search::search_key;
use crate::ids::CategoryId;

/// Deepest level of the tree: a root category is depth 1.
pub const MAX_CATEGORY_DEPTH: usize = 4;
/// Maximum length of a category name.
pub const MAX_CATEGORY_NAME_LEN: usize = 60;

/// A category node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Category {
    /// Category ID.
    pub id: CategoryId,
    /// Parent, or `None` for a root.
    pub parent: Option<CategoryId>,
    /// Name, unique among siblings (case- and accent-insensitive).
    pub name: String,
    /// Optional short code (`[A-Z0-9]{0,8}`).
    pub code: String,
    /// Archived categories are hidden from pickers; products keep them.
    pub archived: bool,
}

/// Validates creating or changing (renaming, moving) `category` in the tree `existing` (which
/// may contain the category's stored version; it is replaced). Checks: name 1–60 chars
/// without surrounding spaces, short code, the parent exists, no cycle, the deepest node of the
/// moved subtree stays within [`MAX_CATEGORY_DEPTH`], and no sibling has the same name.
/// Archived siblings count for name uniqueness, so unarchiving never creates duplicates.
///
/// # Errors
/// The first broken rule.
pub fn validate_category(existing: &[Category], category: &Category) -> Result<(), CatalogError> {
    let len = category.name.chars().count();
    if len == 0 || len > MAX_CATEGORY_NAME_LEN || category.name.trim() != category.name {
        return Err(CatalogError::InvalidField {
            field: "name",
            reason: "1–60 characters without surrounding spaces",
        });
    }
    check_category_code(&category.code)?;
    let parents: HashMap<CategoryId, Option<CategoryId>> = existing
        .iter()
        .filter(|c| c.id != category.id)
        .map(|c| (c.id, c.parent))
        .chain([(category.id, category.parent)])
        .collect();

    // Depth of the category itself: 1 + its ancestors. Walking up from the new parent and
    // meeting the category again is a cycle; a walk longer than the tree is a corrupt cycle above.
    let mut depth = 1;
    let mut cursor = category.parent;
    while let Some(id) = cursor {
        if id == category.id || depth > parents.len() {
            return Err(CatalogError::CategoryCycle);
        }
        cursor = *parents.get(&id).ok_or(CatalogError::UnknownCategory)?;
        depth += 1;
    }

    // Height of the subtree below it (its descendants keep their parents).
    let mut height = 0;
    let mut level = vec![category.id];
    while !level.is_empty() && height <= parents.len() {
        level = parents
            .iter()
            .filter(|(_, p)| p.is_some_and(|p| level.contains(&p)))
            .map(|(id, _)| *id)
            .collect();
        if !level.is_empty() {
            height += 1;
        }
    }
    if depth + height > MAX_CATEGORY_DEPTH {
        return Err(CatalogError::CategoryTooDeep {
            depth: depth + height,
            max: MAX_CATEGORY_DEPTH,
        });
    }

    let key = search_key(&category.name);
    if existing
        .iter()
        .any(|c| c.id != category.id && c.parent == category.parent && search_key(&c.name) == key)
    {
        return Err(CatalogError::DuplicateCategoryName {
            name: category.name.clone(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::CodeKind;
    use uuid::Uuid;

    fn id(n: u128) -> CategoryId {
        CategoryId::from_uuid(Uuid::from_u128(n))
    }

    fn cat(n: u128, parent: Option<u128>, name: &str) -> Category {
        Category {
            id: id(n),
            parent: parent.map(id),
            name: name.to_owned(),
            code: String::new(),
            archived: false,
        }
    }

    /// Food(1) > Drinks(2) > Tea(3) > Green(4); Other(5) > Misc(6).
    fn tree() -> Vec<Category> {
        vec![
            cat(1, None, "Food"),
            cat(2, Some(1), "Drinks"),
            cat(3, Some(2), "Tea"),
            cat(4, Some(3), "Green"),
            cat(5, None, "Other"),
            cat(6, Some(5), "Misc"),
        ]
    }

    #[test]
    fn tree_rules() {
        let too_deep = |depth| Err(CatalogError::CategoryTooDeep { depth, max: 4 });
        let cases = [
            ("new root", cat(10, None, "Toys"), Ok(())),
            ("new at depth 4", cat(10, Some(3), "Black"), Ok(())),
            ("rename in place", cat(4, Some(3), "Green Tea"), Ok(())),
            ("move subtree to root", cat(2, None, "Drinks"), Ok(())),
            ("new at depth 5", cat(10, Some(4), "Matcha"), too_deep(5)),
            (
                "move subtree deeper",
                cat(2, Some(6), "Drinks"),
                too_deep(5),
            ),
            (
                "move under own child",
                cat(2, Some(3), "Drinks"),
                Err(CatalogError::CategoryCycle),
            ),
            (
                "move under itself",
                cat(2, Some(2), "Drinks"),
                Err(CatalogError::CategoryCycle),
            ),
            (
                "unknown parent",
                cat(10, Some(99), "X"),
                Err(CatalogError::UnknownCategory),
            ),
            (
                "sibling name",
                cat(10, None, "food"),
                Err(CatalogError::DuplicateCategoryName {
                    name: "food".into(),
                }),
            ),
            (
                "accented sibling",
                cat(10, Some(1), "DRÍNKS"),
                Err(CatalogError::DuplicateCategoryName {
                    name: "DRÍNKS".into(),
                }),
            ),
            ("same name elsewhere", cat(10, Some(5), "Tea"), Ok(())),
            (
                "empty name",
                cat(10, None, ""),
                Err(CatalogError::InvalidField {
                    field: "name",
                    reason: "1–60 characters without surrounding spaces",
                }),
            ),
            (
                "padded name",
                cat(10, None, " Toys"),
                Err(CatalogError::InvalidField {
                    field: "name",
                    reason: "1–60 characters without surrounding spaces",
                }),
            ),
        ];
        for (name, category, expected) in cases {
            assert_eq!(validate_category(&tree(), &category), expected, "{name}");
        }
        let mut coded = cat(10, None, "Toys");
        coded.code = "to ys".into();
        assert_eq!(
            validate_category(&tree(), &coded),
            Err(CatalogError::InvalidCode {
                kind: CodeKind::CategoryCode,
                value: "to ys".into()
            })
        );
    }

    #[test]
    fn corrupt_cycle_above_is_reported() {
        let looped = vec![cat(1, Some(2), "A"), cat(2, Some(1), "B")];
        assert_eq!(
            validate_category(&looped, &cat(3, Some(1), "C")),
            Err(CatalogError::CategoryCycle)
        );
    }
}
