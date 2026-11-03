//! Translate core query diagnostics at the GUI boundary; core stays language-neutral.
use crate::i18n::{fill, tr};

pub fn message(error: &str) -> String {
    translated(error, tr)
}

fn translated(error: &str, tr: impl Fn(&'static str) -> &'static str) -> String {
    match error {
        "unexpected '>'" => return tr("Unexpected '>'.").into(),
        "missing '>'" => return tr("Missing '>'.").into(),
        "missing closing quote" => return tr("Missing closing quote.").into(),
        "only one of duplicates:, extra-copies:, distinct: and unique: per query" => {
            return tr(
                "Use only one of duplicates:, extra-copies:, distinct: and unique: per query.",
            )
            .into();
        }
        _ => {}
    }
    if let Some(n) = error
        .strip_prefix("a query can have at most ")
        .and_then(|s| s.strip_suffix(" content terms"))
    {
        return fill(tr("A query can have at most {} content terms."), &[&n]);
    }
    if let Some(n) = error
        .strip_prefix("the query is nested too deeply (more than ")
        .and_then(|s| s.strip_suffix(" levels)"))
    {
        return fill(
            tr("The query is nested too deeply (more than {} levels)."),
            &[&n],
        );
    }
    if let Some(value) = error.strip_prefix("bad size: ") {
        return fill(tr("Invalid size: {}"), &[&value]);
    }
    if let Some(value) = error
        .strip_prefix("bad limit: ")
        .and_then(|s| s.strip_suffix(" (use a number, for example limit:100)"))
    {
        return fill(
            tr("Invalid limit: {}. Use a number, for example limit:100."),
            &[&value],
        );
    }
    if let Some(value) = error
        .strip_prefix("bad date: ")
        .and_then(|s| s.strip_suffix(" (use YYYY, YYYY-MM or YYYY-MM-DD)"))
    {
        return fill(
            tr("Invalid date: {}. Use YYYY, YYYY-MM or YYYY-MM-DD."),
            &[&value],
        );
    }
    if let Some(value) = error.strip_prefix("bad time: ").and_then(|s| {
        s.strip_suffix(
            " (use for example <7d, >2h, 2026-10-25, 2026-10, today, lastweek, last3days)",
        )
    }) {
        return fill(
            tr("Invalid time: {}. Use a date or a relative time, for example <7d or today."),
            &[&value],
        );
    }
    if let Some((head, value)) = error
        .strip_prefix("unknown property for ")
        .and_then(|s| s.strip_suffix(" (use content, name, stem, ext, size or modified)"))
        .and_then(|s| s.split_once(": "))
    {
        return fill(
            tr("Unknown property for {0}: {1}. Use content, name, stem, ext, size or modified."),
            &[&head, &value],
        );
    }
    if let Some((head, n)) = error
        .strip_suffix(" properties")
        .and_then(|s| s.split_once(": compares at most "))
    {
        return fill(
            tr("{0}: at most {1} properties can be compared."),
            &[&head, &n],
        );
    }
    if let Some(reason) = error.strip_prefix("bad regular expression: ") {
        // The contents matcher includes a pattern and caret before `error:`.
        let reason = reason
            .lines()
            .rev()
            .find_map(|s| s.trim().strip_prefix("error: "))
            .unwrap_or(reason)
            .trim();
        let localized = match reason {
            "unclosed group" => tr("Unclosed group."),
            "unopened group" => tr("Unopened group."),
            "unclosed character class" => tr("Unclosed character class."),
            "repetition operator missing expression" => {
                tr("Repetition operator is missing an expression.")
            }
            "unrecognized escape sequence" => tr("Unrecognized escape sequence."),
            "the expression is too large" => tr("The expression is too large."),
            _ => return tr("Invalid regular expression.").into(),
        };
        return fill(tr("Invalid regular expression: {}"), &[&localized]);
    }
    tr("Invalid search query.").into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::i18n::{Lang, tr_in};
    use everygrep_core::query::{Query, QueryOptions};

    #[test]
    fn actual_core_errors_are_translated_in_both_catalogs() {
        for query in [
            "content:a content:b content:c content:d content:e content:f content:g",
            "size:invalid",
            "limit:invalid",
            "modified:2026-99-99",
            "modified:nonsense",
            "duplicates:unknown",
            "duplicates: duplicates:",
            "regex:(",
            "\"unfinished",
            ">",
            "<a",
        ] {
            let error = Query::parse(query, QueryOptions::default())
                .err()
                .unwrap()
                .to_string();
            for lang in [Lang::Az, Lang::Tr] {
                let text = translated(&error, |key| tr_in(lang, key));
                assert_ne!(text, message(&error), "{lang:?}: {query}: {error}");
                assert!(
                    !text.contains(&error),
                    "untranslated diagnostic: {lang:?}: {text}"
                );
                assert!(!text.contains("Invalid search query"));
                assert!(!text.contains("a query can have at most"));
                assert!(!text.contains("unclosed group"));
            }
        }
    }
}
