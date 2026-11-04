//! New names for renaming several items at once.
//!
//! A [`Rule`] turns an item's name into its new name: text added before or
//! after it, text or a regular expression replaced, the modification date
//! or a counter added. With `keep_extension` only the part before a file's
//! extension changes. Nothing here touches the file system; checking the
//! names against each other and against the disk is the caller's work.

use regex::{NoExpand, Regex, RegexBuilder};

use crate::localtime::{Zone, civil_from_days};

/// What a rule does.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Mode {
    /// `text` goes before the name.
    #[default]
    Prefix,
    /// `text` goes after the name.
    Suffix,
    /// Every `text` in the name becomes `with`.
    Replace,
    /// Every match of the regular expression `text` becomes `with`, where
    /// `$1` or `${name}` stand for groups.
    Regex,
    /// The modification date, then `text`, before the name (or `text` and
    /// the date after it, with `at_end`).
    Date,
    /// A counter, then `text`, before the name (or after it, with `at_end`).
    Number,
}

/// How a date is written.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DateFormat {
    /// 2026-11-04
    #[default]
    Iso,
    /// 20261104
    Compact,
    /// 2026-11-04 20.15.09 (no colons: they are awkward in file names)
    IsoTime,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rule {
    pub mode: Mode,
    pub text: String,
    pub with: String,
    pub match_case: bool,
    pub keep_extension: bool,
    pub at_end: bool,
    pub date_format: DateFormat,
    pub start: u64,
    pub step: u64,
    /// Digits of the counter, with leading zeros (at least 1).
    pub digits: usize,
}

impl Default for Rule {
    fn default() -> Self {
        Self {
            mode: Mode::Prefix,
            text: String::new(),
            with: String::new(),
            match_case: false,
            keep_extension: true,
            at_end: false,
            date_format: DateFormat::Iso,
            start: 1,
            step: 1,
            digits: 1,
        }
    }
}

/// One item to name.
#[derive(Clone, Copy, Debug)]
pub struct Item<'a> {
    pub name: &'a str,
    pub is_dir: bool,
    /// Modification time, Unix seconds.
    pub modified: i64,
    /// Position in the batch, from 0, for the counter.
    pub position: u64,
}

/// A rule ready to apply.
pub struct Compiled {
    rule: Rule,
    regex: Option<Regex>,
    zone: Zone,
}

/// Why a rule cannot be applied.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RuleError {
    /// The text to find is empty.
    NothingToFind,
    /// The regular expression does not compile; the parser's message.
    BadPattern(String),
}

impl Rule {
    /// Checks the rule once for a whole batch. Dates are written in `zone`.
    pub fn compile(&self, zone: Zone) -> Result<Compiled, RuleError> {
        let regex = match self.mode {
            Mode::Replace | Mode::Regex if self.text.is_empty() => {
                return Err(RuleError::NothingToFind);
            }
            Mode::Replace => Some(
                RegexBuilder::new(&regex::escape(&self.text))
                    .case_insensitive(!self.match_case)
                    .build()
                    .map_err(|e| RuleError::BadPattern(e.to_string()))?,
            ),
            Mode::Regex => Some(
                RegexBuilder::new(&self.text)
                    .case_insensitive(!self.match_case)
                    .size_limit(1 << 20)
                    .build()
                    .map_err(|e| RuleError::BadPattern(e.to_string()))?,
            ),
            _ => None,
        };
        Ok(Compiled {
            rule: self.clone(),
            regex,
            zone,
        })
    }
}

/// Bytes of `name` before its extension; folders and names without one
/// are taken whole, and so is a name whose only dot leads it (`.bashrc`).
pub fn stem_len(name: &str, is_dir: bool) -> usize {
    if is_dir {
        return name.len();
    }
    match name.rfind('.') {
        Some(dot) if dot > 0 => dot,
        _ => name.len(),
    }
}

impl Compiled {
    /// The new name of `item`.
    pub fn apply(&self, item: Item<'_>) -> String {
        let r = &self.rule;
        let split = if r.keep_extension {
            stem_len(item.name, item.is_dir)
        } else {
            item.name.len()
        };
        let (stem, ext) = item.name.split_at(split);
        let new_stem = match r.mode {
            Mode::Prefix => format!("{}{stem}", r.text),
            Mode::Suffix => format!("{stem}{}", r.text),
            Mode::Replace => self
                .regex
                .as_ref()
                .expect("compiled")
                .replace_all(stem, NoExpand(&r.with))
                .into_owned(),
            Mode::Regex => self
                .regex
                .as_ref()
                .expect("compiled")
                .replace_all(stem, r.with.as_str())
                .into_owned(),
            Mode::Date => self.around(stem, &self.date(item.modified)),
            Mode::Number => {
                let n = r.start.saturating_add(r.step.saturating_mul(item.position));
                self.around(stem, &format!("{n:0width$}", width = r.digits.max(1)))
            }
        };
        format!("{new_stem}{ext}")
    }

    /// `value` and the separator `text` before `stem`, or after it.
    fn around(&self, stem: &str, value: &str) -> String {
        let sep = &self.rule.text;
        if self.rule.at_end {
            format!("{stem}{sep}{value}")
        } else {
            format!("{value}{sep}{stem}")
        }
    }

    fn date(&self, unix: i64) -> String {
        let local = self.zone.to_local(unix);
        let (y, m, d) = civil_from_days(local.div_euclid(86_400));
        let secs = local.rem_euclid(86_400);
        match self.rule.date_format {
            DateFormat::Iso => format!("{y:04}-{m:02}-{d:02}"),
            DateFormat::Compact => format!("{y:04}{m:02}{d:02}"),
            DateFormat::IsoTime => format!(
                "{y:04}-{m:02}-{d:02} {:02}.{:02}.{:02}",
                secs / 3600,
                secs / 60 % 60,
                secs % 60
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(name: &str, position: u64) -> Item<'_> {
        Item {
            name,
            is_dir: false,
            modified: 1_762_286_109, // 2025-11-04 19:55:09 UTC
            position,
        }
    }

    fn apply(rule: Rule, name: &str) -> String {
        rule.compile(Zone::utc()).unwrap().apply(item(name, 0))
    }

    #[test]
    fn text_goes_before_or_after_the_name_and_keeps_the_extension() {
        let prefix = Rule {
            text: "2024 ".into(),
            ..Rule::default()
        };
        assert_eq!(apply(prefix.clone(), "photo.jpg"), "2024 photo.jpg");
        let suffix = Rule {
            mode: Mode::Suffix,
            text: "-old".into(),
            ..Rule::default()
        };
        assert_eq!(apply(suffix.clone(), "photo.jpg"), "photo-old.jpg");
        assert_eq!(
            apply(suffix.clone(), "archive.tar.gz"),
            "archive.tar-old.gz"
        );
        assert_eq!(apply(suffix.clone(), ".bashrc"), ".bashrc-old");
        let whole = Rule {
            keep_extension: false,
            ..suffix
        };
        assert_eq!(apply(whole, "photo.jpg"), "photo.jpg-old");
        let folder = Item {
            is_dir: true,
            ..item("v1.2", 0)
        };
        let suffix = Rule {
            mode: Mode::Suffix,
            text: "-x".into(),
            ..Rule::default()
        };
        assert_eq!(suffix.compile(Zone::utc()).unwrap().apply(folder), "v1.2-x");
    }

    #[test]
    fn replace_is_literal_and_follows_match_case() {
        let rule = Rule {
            mode: Mode::Replace,
            text: "a.b".into(),
            with: "$1x".into(),
            ..Rule::default()
        };
        // The dot is not a pattern and `$1` is not a group here.
        assert_eq!(apply(rule.clone(), "a.b axb A.B.txt"), "$1x axb $1x.txt");
        let case = Rule {
            match_case: true,
            ..rule
        };
        assert_eq!(apply(case, "a.b A.B.txt"), "$1x A.B.txt");
    }

    #[test]
    fn regular_expressions_use_groups_and_report_bad_patterns() {
        let rule = Rule {
            mode: Mode::Regex,
            text: r"(\d+)-(\w+)".into(),
            with: "${2}_$1".into(),
            ..Rule::default()
        };
        assert_eq!(apply(rule.clone(), "12-abc.txt"), "abc_12.txt");
        let bad = Rule {
            text: "(".into(),
            ..rule.clone()
        };
        assert!(matches!(
            bad.compile(Zone::utc()),
            Err(RuleError::BadPattern(_))
        ));
        let empty = Rule {
            text: String::new(),
            ..rule
        };
        assert_eq!(
            empty.compile(Zone::utc()).err(),
            Some(RuleError::NothingToFind)
        );
    }

    #[test]
    fn dates_and_counters_go_before_or_after_with_the_separator() {
        let date = Rule {
            mode: Mode::Date,
            text: " ".into(),
            ..Rule::default()
        };
        assert_eq!(apply(date.clone(), "a.jpg"), "2025-11-04 a.jpg");
        let compact_end = Rule {
            date_format: DateFormat::Compact,
            at_end: true,
            text: "_".into(),
            ..date.clone()
        };
        assert_eq!(apply(compact_end, "a.jpg"), "a_20251104.jpg");
        let with_time = Rule {
            date_format: DateFormat::IsoTime,
            ..date
        };
        assert_eq!(apply(with_time, "a.jpg"), "2025-11-04 19.55.09 a.jpg");
        let number = Rule {
            mode: Mode::Number,
            text: " - ".into(),
            start: 9,
            step: 2,
            digits: 3,
            ..Rule::default()
        };
        let c = number.compile(Zone::utc()).unwrap();
        assert_eq!(c.apply(item("x.png", 0)), "009 - x.png");
        assert_eq!(c.apply(item("x.png", 1)), "011 - x.png");
        let huge = Rule {
            start: u64::MAX,
            ..number
        };
        assert_eq!(
            huge.compile(Zone::utc()).unwrap().apply(item("x", 5)),
            format!("{} - x", u64::MAX)
        );
    }

    #[test]
    fn dates_before_1970_are_written_too() {
        let rule = Rule {
            mode: Mode::Date,
            text: " ".into(),
            ..Rule::default()
        };
        let c = rule.compile(Zone::utc()).unwrap();
        let old = Item {
            modified: -86_400,
            ..item("a", 0)
        };
        assert_eq!(c.apply(old), "1969-12-31 a");
    }
}
