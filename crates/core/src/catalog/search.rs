//! The `search_key` column: lower-cased text with accents removed, because text in the
//! database compares byte-wise (spec 2.2 §2, 2.1 decision).
//!
//! Folding covers Latin-1 Supplement and Latin Extended-A, which is enough for Indonesian and
//! the European names that show up in brand and product names. Other characters are kept
//! lower-cased as they are.

/// Lower-cases `text`, removes accents, trims it and collapses runs of whitespace to one space.
/// Also used as the comparison key for "unique, case-insensitive" names (units, options,
/// sibling categories).
#[must_use]
pub fn search_key(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut pending_space = false;
    for c in text.chars() {
        if c.is_whitespace() {
            pending_space = !out.is_empty();
            continue;
        }
        if pending_space {
            out.push(' ');
            pending_space = false;
        }
        for lower in c.to_lowercase() {
            match fold(lower) {
                Some(ascii) => out.push_str(ascii),
                None => out.push(lower),
            }
        }
    }
    out
}

/// The product's `search_key`: name, receipt name, code and brand joined by spaces
/// (spec 2.2 §2).
#[must_use]
pub fn product_search_key(
    name: &str,
    receipt_name: &str,
    code: &str,
    brand: Option<&str>,
) -> String {
    let parts = [name, receipt_name, code, brand.unwrap_or("")];
    search_key(&parts.join(" "))
}

/// The ASCII replacement of a lower-case accented letter, if any.
const fn fold(c: char) -> Option<&'static str> {
    Some(match c {
        'à'..='å' | '\u{100}'..='\u{105}' => "a",
        'æ' => "ae",
        'ç' | '\u{106}'..='\u{10D}' => "c",
        'ð' | '\u{10E}'..='\u{111}' => "d",
        'è'..='ë' | '\u{112}'..='\u{11B}' => "e",
        '\u{11C}'..='\u{123}' => "g",
        '\u{124}'..='\u{127}' => "h",
        'ì'..='ï' | '\u{128}'..='\u{131}' => "i",
        '\u{132}'..='\u{133}' => "ij",
        '\u{134}'..='\u{135}' => "j",
        '\u{136}'..='\u{138}' => "k",
        '\u{139}'..='\u{142}' => "l",
        'ñ' | '\u{143}'..='\u{14B}' => "n",
        'ò'..='ö' | 'ø' | '\u{14C}'..='\u{151}' => "o",
        '\u{152}'..='\u{153}' => "oe",
        '\u{154}'..='\u{159}' => "r",
        'ß' => "ss",
        '\u{15A}'..='\u{161}' | '\u{17F}' => "s",
        '\u{162}'..='\u{167}' => "t",
        'þ' => "th",
        'ù'..='ü' | '\u{168}'..='\u{173}' => "u",
        '\u{174}'..='\u{175}' => "w",
        'ý' | 'ÿ' | '\u{176}'..='\u{178}' => "y",
        '\u{179}'..='\u{17E}' => "z",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folds_case_accents_and_whitespace() {
        let cases = [
            ("Kaos Polos", "kaos polos"),
            ("  Café   Crème\tBrûlée ", "cafe creme brulee"),
            ("ÀÉÎÕÜ Ñ Ç Ý Ÿ", "aeiou n c y y"),
            ("Æsir Œuvre straße Þór Ðoð", "aesir oeuvre strasse thor dod"),
            (
                "Łódź Škoda Żubr ĳssel Ĝ Ħ Ĵ Ķ Ŕ Ţ Ŵ Ŷ ſ Ø",
                "lodz skoda zubr ijssel g h j k r t w y s o",
            ),
            ("Ŋ Ő Ű Ā Ē Ī", "n o u a e i"),
            ("× ÷ 日本 123", "× ÷ 日本 123"),
            ("", ""),
            ("   ", ""),
        ];
        for (input, key) in cases {
            assert_eq!(search_key(input), key, "{input:?}");
        }
    }

    #[test]
    fn product_key_joins_fields() {
        assert_eq!(
            product_search_key("Kaos Polos", "KAOS POLOS", "KAOS01", Some("Énergie")),
            "kaos polos kaos polos kaos01 energie"
        );
        assert_eq!(
            product_search_key("Teh", "Teh", "P000001", None),
            "teh teh p000001"
        );
    }
}
