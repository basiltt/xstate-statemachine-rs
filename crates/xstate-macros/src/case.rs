//! Identifier case conventions for `rename_all`.

use syn::LitStr;

/// A naming convention applied to Rust variant names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Case {
    /// `order_paid`
    Snake,
    /// `ORDER_PAID`
    ScreamingSnake,
    /// `order-paid`
    Kebab,
    /// `orderPaid`
    Camel,
    /// `OrderPaid`
    Pascal,
}

/// Every accepted spelling, for error messages.
const NAMES: &str = "\"snake_case\", \"SCREAMING_SNAKE_CASE\", \"kebab-case\", \"camelCase\" or \
                     \"PascalCase\"";

impl Case {
    /// Parse a `rename_all = "..."` value.
    pub(crate) fn from_lit(lit: &LitStr) -> syn::Result<Self> {
        Ok(match lit.value().as_str() {
            "snake_case" => Self::Snake,
            "SCREAMING_SNAKE_CASE" => Self::ScreamingSnake,
            "kebab-case" => Self::Kebab,
            "camelCase" => Self::Camel,
            "PascalCase" => Self::Pascal,
            other => {
                let msg = format!("unknown case convention `{other}`; expected {NAMES}");
                return Err(syn::Error::new(lit.span(), msg));
            }
        })
    }

    /// Re-spell `ident` (a Rust identifier in any case) in this convention.
    pub(crate) fn apply(self, ident: &str) -> String {
        let words = split_words(ident);
        match self {
            Self::Snake => join(&words, "_", str::to_lowercase),
            Self::ScreamingSnake => join(&words, "_", str::to_uppercase),
            Self::Kebab => join(&words, "-", str::to_lowercase),
            Self::Pascal => join(&words, "", capitalize),
            Self::Camel => {
                let mut out = join(&words, "", capitalize);
                if let Some(first) = out.chars().next() {
                    out.replace_range(..first.len_utf8(), &first.to_lowercase().to_string());
                }
                out
            }
        }
    }
}

fn join(words: &[String], sep: &str, f: fn(&str) -> String) -> String {
    words.iter().map(|w| f(w)).collect::<Vec<_>>().join(sep)
}

fn capitalize(word: &str) -> String {
    let mut chars = word.chars();
    chars.next().map_or_else(String::new, |c| {
        c.to_uppercase().chain(chars.flat_map(char::to_lowercase)).collect()
    })
}

/// Split `HTTPRequest2Sent` / `order_paid` / `orderPaid` into words: underscores separate,
/// a lower-to-upper step starts a word, and an acronym ends before its last capital when a
/// lowercase letter follows (`HTTPRequest` is `HTTP` + `Request`).
fn split_words(ident: &str) -> Vec<String> {
    let ident = ident.strip_prefix("r#").unwrap_or(ident);
    let mut words = Vec::new();
    for part in ident.split('_').filter(|p| !p.is_empty()) {
        let chars: Vec<char> = part.chars().collect();
        let mut current = String::new();
        for (i, &c) in chars.iter().enumerate() {
            let prev = i.checked_sub(1).map(|p| chars[p]);
            let next = chars.get(i + 1).copied();
            let boundary = c.is_uppercase()
                && prev.is_some_and(|p| {
                    p.is_lowercase()
                        || p.is_ascii_digit()
                        || (p.is_uppercase() && next.is_some_and(char::is_lowercase))
                });
            if boundary && !current.is_empty() {
                words.push(std::mem::take(&mut current));
            }
            current.push(c);
        }
        if !current.is_empty() {
            words.push(current);
        }
    }
    words
}

#[cfg(test)]
mod tests {
    use super::Case;

    #[test]
    fn converts_between_conventions() {
        assert_eq!(Case::Snake.apply("OrderPaid"), "order_paid");
        assert_eq!(Case::ScreamingSnake.apply("OrderPaid"), "ORDER_PAID");
        assert_eq!(Case::Kebab.apply("HTTPRequestSent"), "http-request-sent");
        assert_eq!(Case::Camel.apply("OrderPaid"), "orderPaid");
        assert_eq!(Case::Pascal.apply("order_paid"), "OrderPaid");
        assert_eq!(Case::Snake.apply("Step2Done"), "step2_done");
        assert_eq!(Case::Snake.apply("A"), "a");
    }
}
