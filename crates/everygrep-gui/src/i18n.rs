//! Translations of the window's texts.
//!
//! English is the source language: every text is written in English in the
//! code, and the translations are gettext catalogs in
//! `translations/<lang>/LC_MESSAGES/everygrep-gui.po`. The same files serve
//! both halves of the window:
//!
//! - Texts in `ui/*.slint` are marked with `@tr(...)`. The build script hands
//!   the catalogs to the Slint compiler (`with_bundled_translations`), which
//!   compiles them into the program; [`apply_to_ui`] picks the language with
//!   `slint::select_bundled_translation`, and every `@tr` binding updates.
//! - Texts made in Rust go through [`tr`] and [`trn`], which look them up in
//!   the same catalogs, embedded with `include_str!` and read on first use.
//!
//! Placeholders are `{}` (the next argument) or `{0}`, `{1}` (by position),
//! as in Slint's `@tr`; [`fill`] puts the arguments in. A test checks that
//! every text marked in the sources has a translation in every catalog, with
//! the same placeholders, and that no text is left in English: a
//! `# TODO: translate` comment or a translation equal to the English text
//! fails, except for the words listed as the same in that language.
//!
//! Azerbaijani and Turkish nouns stay singular after a number ("12 fayl",
//! "12 dosya"); their catalogs still have the two gettext plural forms
//! (`plural=(n != 1)`), so a text for exactly one can be worded apart
//! ("Ad kopyalandı." for one name).

use std::collections::HashMap;
use std::fmt::Display;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU8, Ordering};

/// A language the window can be shown in.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Lang {
    #[default]
    En,
    Az,
    Tr,
}

impl Lang {
    pub const ALL: [Lang; 3] = [Lang::En, Lang::Az, Lang::Tr];

    /// Language code: the catalog folder name and the settings value.
    pub fn code(self) -> &'static str {
        match self {
            Lang::En => "en",
            Lang::Az => "az",
            Lang::Tr => "tr",
        }
    }

    /// The language's name in that language, for the language choice.
    pub fn native_name(self) -> &'static str {
        match self {
            Lang::En => "English",
            Lang::Az => "Azərbaycanca",
            Lang::Tr => "Türkçe",
        }
    }

    /// The language of a locale name such as `az`, `az_AZ.UTF-8`, `tr-TR`
    /// or `en_GB@euro`; `None` for a language without a translation.
    pub fn from_locale(locale: &str) -> Option<Lang> {
        let lang = locale
            .split(['_', '-', '.', '@'])
            .next()
            .unwrap_or("")
            .to_ascii_lowercase();
        Lang::ALL.into_iter().find(|l| l.code() == lang)
    }

    fn catalog_index(self) -> Option<usize> {
        match self {
            Lang::En => None,
            Lang::Az => Some(0),
            Lang::Tr => Some(1),
        }
    }
}

/// The language the desktop session asks for, read like gettext does:
/// `LC_ALL`, then `LC_MESSAGES`, then `LANG` give the locale; unless that
/// locale is `C` or `POSIX`, the colon-separated list in `LANGUAGE` comes
/// first. The first entry with a translation wins; English otherwise.
pub fn system_language(var: impl Fn(&str) -> Option<String>) -> Lang {
    let set = |name: &str| var(name).filter(|v| !v.is_empty());
    let locale = set("LC_ALL")
        .or_else(|| set("LC_MESSAGES"))
        .or_else(|| set("LANG"));
    if matches!(locale.as_deref(), Some("C" | "POSIX"))
        || locale.as_deref().is_some_and(|l| l.starts_with("C."))
    {
        return Lang::En;
    }
    let listed = set("LANGUAGE").unwrap_or_default();
    listed
        .split(':')
        .chain(locale.as_deref())
        .find_map(Lang::from_locale)
        .unwrap_or(Lang::En)
}

/// The language of the running program's environment.
pub fn system_language_from_env() -> Lang {
    system_language(|name| std::env::var(name).ok())
}

static CURRENT: AtomicU8 = AtomicU8::new(0);

/// The language texts are shown in.
pub fn current() -> Lang {
    match CURRENT.load(Ordering::Relaxed) {
        1 => Lang::Az,
        2 => Lang::Tr,
        _ => Lang::En,
    }
}

/// Shows the texts made in Rust in `lang` from now on. Call [`apply_to_ui`]
/// too for the texts of the `.slint` files.
pub fn set(lang: Lang) {
    CURRENT.store(lang as u8, Ordering::Relaxed);
}

/// Shows the `.slint` texts in the current language. Needs a window to
/// exist, since the compiled catalogs are registered when the first one is
/// created.
pub fn apply_to_ui() {
    let code = match current() {
        Lang::En => "",
        other => other.code(),
    };
    if let Err(e) = slint::select_bundled_translation(code) {
        eprintln!("everygrep-gui: cannot select the language {code}: {e}");
    }
}

/// The catalogs, in [`Lang::catalog_index`] order.
const CATALOGS: [&str; 2] = [
    include_str!("../translations/az/LC_MESSAGES/everygrep-gui.po"),
    include_str!("../translations/tr/LC_MESSAGES/everygrep-gui.po"),
];

/// One message of a catalog.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Message {
    pub msgid: String,
    pub plural: Option<String>,
    /// One text, or one per plural form; empty texts are untranslated.
    pub msgstr: Vec<String>,
    /// Marked `#, fuzzy`: not to be used.
    pub fuzzy: bool,
}

/// Reads the messages of a gettext `.po` file: `msgid`, `msgid_plural`,
/// `msgstr` and `msgstr[n]`, each with continuation lines. Comments are
/// skipped except for the fuzzy flag; `msgctxt` is not used by EveryGrep
/// and refused. The header (empty `msgid`) is returned as the first
/// message.
pub fn parse_po(text: &str) -> Result<Vec<Message>, String> {
    #[derive(PartialEq)]
    enum Field {
        None,
        Id,
        Plural,
        Str(usize),
    }
    let mut out: Vec<Message> = Vec::new();
    let mut cur = Message::default();
    let mut field = Field::None;
    let mut fuzzy_next = false;
    let mut started = false;
    let finish = |cur: &mut Message, out: &mut Vec<Message>, started: &mut bool| {
        if *started {
            out.push(std::mem::take(cur));
        }
        *started = false;
    };
    for (i, raw) in text.lines().enumerate() {
        let line = raw.trim();
        let at = |e: &str| format!("line {}: {e}", i + 1);
        if line.is_empty() {
            continue;
        }
        if let Some(comment) = line.strip_prefix('#') {
            if comment.starts_with(',') && comment.contains("fuzzy") {
                fuzzy_next = true;
            }
            continue;
        }
        if line.starts_with('"') {
            let s = unquote(line).ok_or_else(|| at("bad string"))?;
            match field {
                Field::Id => cur.msgid.push_str(&s),
                Field::Plural => cur.plural.get_or_insert_default().push_str(&s),
                Field::Str(n) => cur.msgstr[n].push_str(&s),
                Field::None => return Err(at("string outside a message")),
            }
            continue;
        }
        let (key, rest) = line
            .split_once(' ')
            .ok_or_else(|| at("expected a keyword"))?;
        let s = unquote(rest.trim()).ok_or_else(|| at("bad string"))?;
        match key {
            "msgctxt" => return Err(at("msgctxt is not used")),
            "msgid" => {
                finish(&mut cur, &mut out, &mut started);
                started = true;
                cur.fuzzy = std::mem::take(&mut fuzzy_next);
                cur.msgid = s;
                field = Field::Id;
            }
            "msgid_plural" if field == Field::Id => {
                cur.plural = Some(s);
                field = Field::Plural;
            }
            _ if key == "msgstr" || key.starts_with("msgstr[") => {
                if !started {
                    return Err(at("msgstr without msgid"));
                }
                let n = match key.strip_prefix("msgstr[") {
                    Some(n) => n
                        .strip_suffix(']')
                        .and_then(|n| n.parse::<usize>().ok())
                        .ok_or_else(|| at("bad plural index"))?,
                    None => 0,
                };
                if n != cur.msgstr.len() {
                    return Err(at("plural forms out of order"));
                }
                cur.msgstr.push(s);
                field = Field::Str(n);
            }
            _ => return Err(at("unknown keyword")),
        }
    }
    finish(&mut cur, &mut out, &mut started);
    Ok(out)
}

/// The text of a quoted `.po` string, with `\"`, `\\`, `\n` and `\t`
/// unescaped.
fn unquote(s: &str) -> Option<String> {
    let inner = s.strip_prefix('"')?.strip_suffix('"')?;
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next()? {
                'n' => out.push('\n'),
                't' => out.push('\t'),
                '"' => out.push('"'),
                '\\' => out.push('\\'),
                _ => return None,
            }
        } else if c == '"' {
            return None;
        } else {
            out.push(c);
        }
    }
    Some(out)
}

/// Translations by English text: one text, or one per plural form.
type Catalog = HashMap<String, Vec<String>>;

fn catalogs() -> &'static [Catalog; 2] {
    static LOADED: OnceLock<[Catalog; 2]> = OnceLock::new();
    LOADED.get_or_init(|| {
        CATALOGS.map(|text| {
            parse_po(text)
                .unwrap_or_default()
                .into_iter()
                .filter(|m| {
                    !m.msgid.is_empty() && !m.fuzzy && m.msgstr.iter().all(|s| !s.is_empty())
                })
                .map(|m| (m.msgid, m.msgstr))
                .collect()
        })
    })
}

/// `text` in `lang`; the English text when there is no translation.
pub fn tr_in(lang: Lang, text: &'static str) -> &'static str {
    lang.catalog_index()
        .and_then(|i| catalogs()[i].get(text))
        .and_then(|forms| forms.first())
        .map_or(text, String::as_str)
}

/// `text` in the current language.
pub fn tr(text: &'static str) -> &'static str {
    tr_in(current(), text)
}

/// The text for the number `n` in `lang`: `one` or `many` in English, the
/// matching plural form of a translation. All three languages use the
/// gettext rule `n != 1`.
pub fn trn_in(lang: Lang, one: &'static str, many: &'static str, n: usize) -> &'static str {
    let form = usize::from(n != 1);
    match lang.catalog_index().and_then(|i| catalogs()[i].get(one)) {
        Some(forms) => forms.get(form).or(forms.last()).map_or(one, String::as_str),
        None if form == 0 => one,
        None => many,
    }
}

/// [`trn_in`] in the current language.
pub fn trn(one: &'static str, many: &'static str, n: usize) -> &'static str {
    trn_in(current(), one, many, n)
}

/// Puts `args` into the placeholders of `template`: `{}` takes the next
/// argument, `{0}`, `{1}` the one at that position. A placeholder without
/// an argument is left as it is.
pub fn fill(template: &str, args: &[&dyn Display]) -> String {
    let mut out = String::with_capacity(template.len() + 16);
    let mut next = 0;
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        let Some(close) = after.find('}') else {
            out.push_str(&rest[open..]);
            return out;
        };
        let inner = &after[..close];
        let index = if inner.is_empty() {
            next += 1;
            Some(next - 1)
        } else {
            inner.parse::<usize>().ok()
        };
        match index.and_then(|i| args.get(i)) {
            Some(arg) => out.push_str(&arg.to_string()),
            None => out.push_str(&rest[open..open + close + 2]),
        }
        rest = &after[close + 1..];
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use std::path::Path;

    /// Texts that stay the same as the English one in each language: names,
    /// units and words the language takes over unchanged. Any other
    /// translation equal to the English text is an untranslated placeholder.
    const SAME_AS_ENGLISH: [&[&str]; 2] = [
        &[
            "Audio",
            "Video",
            "Regex",
            "UUID {}",
            "MD5",
            "SHA-256",
            "CRC-32",
            "KiB",
            "MiB",
            "GiB",
            "YYYY-MM-DD",
        ],
        &[
            "Mar",
            "May",
            "Video",
            "Program",
            "Regex",
            "UUID {}",
            "MD5",
            "SHA-256",
            "CRC-32",
            "KiB",
            "MiB",
            "GiB",
            "YYYY-MM-DD",
        ],
    ];

    #[test]
    fn locale_names() {
        assert_eq!(Lang::from_locale("az_AZ.UTF-8"), Some(Lang::Az));
        assert_eq!(Lang::from_locale("tr-TR"), Some(Lang::Tr));
        assert_eq!(Lang::from_locale("TR"), Some(Lang::Tr));
        assert_eq!(Lang::from_locale("en_GB@euro"), Some(Lang::En));
        assert_eq!(Lang::from_locale("de_DE.UTF-8"), None);
        assert_eq!(Lang::from_locale(""), None);
    }

    fn env<'a>(vars: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |name| {
            vars.iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| (*v).to_owned())
        }
    }

    #[test]
    fn system_language_follows_gettext_order() {
        assert_eq!(system_language(env(&[])), Lang::En);
        assert_eq!(system_language(env(&[("LANG", "az_AZ.UTF-8")])), Lang::Az);
        assert_eq!(
            system_language(env(&[
                ("LANG", "az_AZ.UTF-8"),
                ("LC_MESSAGES", "tr_TR.UTF-8")
            ])),
            Lang::Tr
        );
        assert_eq!(
            system_language(env(&[("LC_ALL", "en_US.UTF-8"), ("LANG", "az_AZ.UTF-8")])),
            Lang::En
        );
        // LANGUAGE comes first, its first known entry wins.
        assert_eq!(
            system_language(env(&[("LANGUAGE", "de:tr:az"), ("LANG", "en_US.UTF-8")])),
            Lang::Tr
        );
        assert_eq!(
            system_language(env(&[("LANGUAGE", "de"), ("LANG", "az_AZ.UTF-8")])),
            Lang::Az
        );
        // The C locale turns translations off, as in gettext.
        assert_eq!(
            system_language(env(&[("LANGUAGE", "az"), ("LC_ALL", "C")])),
            Lang::En
        );
        assert_eq!(
            system_language(env(&[("LANGUAGE", "az"), ("LANG", "C.UTF-8")])),
            Lang::En
        );
        assert_eq!(system_language(env(&[("LANG", "de_DE.UTF-8")])), Lang::En);
    }

    #[test]
    fn placeholders_are_filled() {
        assert_eq!(fill("{} of {}", &[&1, &"2"]), "1 of 2");
        assert_eq!(fill("{1} / {0}", &[&"a", &"b"]), "b / a");
        assert_eq!(fill("{} {", &[&1]), "1 {");
        assert_eq!(fill("{} {}", &[&1]), "1 {}");
        assert_eq!(fill("none", &[]), "none");
    }

    #[test]
    fn po_files_are_read() {
        let text = "# c\nmsgid \"\"\nmsgstr \"\"\n\"Plural-Forms: nplurals=2; plural=(n != 1);\\n\"\n\n\
                    #, fuzzy\nmsgid \"a\"\nmsgstr \"b\"\n\nmsgid \"x \\\"q\\\"\"\n\"y\"\nmsgid_plural \"xs\"\n\
                    msgstr[0] \"1\"\nmsgstr[1] \"2\"\n";
        let m = parse_po(text).unwrap();
        assert_eq!(m.len(), 3);
        assert!(m[0].msgstr[0].contains("Plural-Forms"));
        assert!(m[1].fuzzy);
        assert_eq!(m[2].msgid, "x \"q\"y");
        assert_eq!(m[2].plural.as_deref(), Some("xs"));
        assert_eq!(m[2].msgstr, ["1", "2"]);
        assert!(parse_po("msgctxt \"c\"\nmsgid \"a\"\nmsgstr \"b\"").is_err());
        assert!(parse_po("msgstr \"b\"").is_err());
    }

    #[test]
    fn lookups_fall_back_to_english() {
        assert_eq!(tr_in(Lang::En, "Open"), "Open");
        assert_eq!(tr_in(Lang::Az, "Open"), "Aç");
        assert_eq!(tr_in(Lang::Tr, "Open"), "Aç");
        assert_eq!(tr_in(Lang::Az, "not a message"), "not a message");
        assert_eq!(trn_in(Lang::En, "{} item", "{} items", 1), "{} item");
        assert_eq!(trn_in(Lang::En, "{} item", "{} items", 2), "{} items");
        assert_eq!(trn_in(Lang::Tr, "{} item", "{} items", 2), "{} öğe");
        assert_eq!(trn_in(Lang::Az, "{} item", "{} items", 1), "{} element");
        assert_eq!(trn_in(Lang::Az, "x", "xs", 5), "xs");
    }

    /// A string literal of Rust or Slint starting at `s` (after the opening
    /// quote): its text and the rest after the closing quote.
    fn literal(s: &str) -> (String, &str) {
        let mut out = String::new();
        let mut chars = s.char_indices();
        while let Some((i, c)) = chars.next() {
            match c {
                '"' => return (out, &s[i + 1..]),
                '\\' => match chars.next().map(|(_, c)| c) {
                    Some('n') => out.push('\n'),
                    Some('t') => out.push('\t'),
                    Some(c) => out.push(c),
                    None => break,
                },
                c => out.push(c),
            }
        }
        panic!("unterminated string: {s}");
    }

    /// (text, plural) pairs marked for translation in one source file:
    /// `@tr("…")` and `@tr("…" | "…" % n)` in Slint, `tr("…")` and
    /// `trn("…", "…", n)` in Rust. Calls whose first argument is not a
    /// string literal are skipped.
    fn marked(source: &str, slint: bool) -> Vec<(String, Option<String>)> {
        let mut out = Vec::new();
        let calls: &[&str] = if slint { &["@tr("] } else { &["tr(", "trn("] };
        for call in calls {
            let mut rest = source;
            while let Some(at) = rest.find(call) {
                let before = rest[..at].chars().next_back();
                let start = &rest[at + call.len()..];
                rest = start;
                if !slint && before.is_some_and(|c| c.is_alphanumeric() || c == '_' || c == '@') {
                    continue;
                }
                let Some(start) = start.trim_start().strip_prefix('"') else {
                    continue;
                };
                let (text, after) = literal(start);
                let after = after.trim_start();
                let separator = if slint { '|' } else { ',' };
                let plural = after
                    .strip_prefix(separator)
                    .filter(|_| slint || *call == "trn(")
                    .map(|p| {
                        let p = p.trim_start().strip_prefix('"').expect("plural text");
                        literal(p).0
                    });
                out.push((text, plural));
            }
        }
        out
    }

    /// Every text marked for translation in the window's sources.
    fn sources() -> BTreeSet<(String, Option<String>)> {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut out = BTreeSet::new();
        for (dir, ext) in [("ui", "slint"), ("src", "rs")] {
            for entry in std::fs::read_dir(root.join(dir)).unwrap() {
                let path = entry.unwrap().path();
                // This file's tests hold examples, not texts of the window.
                if path.extension().is_some_and(|e| e == ext) && !path.ends_with("i18n.rs") {
                    let text = std::fs::read_to_string(&path).unwrap();
                    out.extend(marked(&text, ext == "slint"));
                }
            }
        }
        out
    }

    /// Placeholders of a text: `{}`, `{0}`, `{n}`, sorted.
    fn placeholders(s: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut rest = s;
        while let Some(open) = rest.find('{') {
            let Some(close) = rest[open..].find('}') else {
                break;
            };
            out.push(rest[open..open + close + 1].to_owned());
            rest = &rest[open + close + 1..];
        }
        out.sort();
        out
    }

    #[test]
    fn sources_are_found() {
        let s = sources();
        assert!(s.len() > 200, "only {} texts found", s.len());
        assert!(s.contains(&("Open containing folder".to_owned(), None)));
        assert!(s.contains(&("{} item".to_owned(), Some("{} items".to_owned()))));
        assert!(s.contains(&("Language".to_owned(), None)));
    }

    #[test]
    fn every_text_is_translated() {
        let sources = sources();
        for (i, lang) in [Lang::Az, Lang::Tr].into_iter().enumerate() {
            for (n, line) in CATALOGS[i].lines().enumerate() {
                assert!(
                    !(line.starts_with('#') && line.contains("TODO")),
                    "{lang:?}: line {} is marked as not translated yet",
                    n + 1
                );
            }
            let same_as_english = SAME_AS_ENGLISH[i];
            let mut same_seen = BTreeSet::new();
            let messages = parse_po(CATALOGS[i]).unwrap_or_else(|e| panic!("{lang:?}: {e}"));
            let header = &messages[0];
            assert!(header.msgid.is_empty(), "{lang:?}: header first");
            assert!(
                header.msgstr[0].contains("Plural-Forms: nplurals=2; plural=(n != 1);"),
                "{lang:?}: plural rule"
            );
            let mut seen = BTreeSet::new();
            for m in &messages[1..] {
                let key = (m.msgid.clone(), m.plural.clone());
                assert!(seen.insert(key.clone()), "{lang:?}: {key:?} twice");
                assert!(!m.fuzzy, "{lang:?}: {key:?} is fuzzy");
                assert!(sources.contains(&key), "{lang:?}: {key:?} is not used");
                let forms = if m.plural.is_some() { 2 } else { 1 };
                assert_eq!(m.msgstr.len(), forms, "{lang:?}: {key:?} forms");
                for (n, text) in m.msgstr.iter().enumerate() {
                    assert!(!text.is_empty(), "{lang:?}: {key:?} is not translated");
                    let english = if n == 0 {
                        &m.msgid
                    } else {
                        m.plural.as_ref().unwrap()
                    };
                    assert_eq!(
                        placeholders(text),
                        placeholders(english),
                        "{lang:?}: placeholders of {key:?} form {n}"
                    );
                    if text == english {
                        assert!(
                            same_as_english.contains(&english.as_str()),
                            "{lang:?}: {key:?} form {n} is still in English"
                        );
                        same_seen.insert(english.as_str());
                    }
                }
            }
            for word in same_as_english {
                assert!(
                    same_seen.contains(word),
                    "{lang:?}: {word:?} is listed as the same as English but is not"
                );
            }
            let missing: Vec<_> = sources.difference(&seen).collect();
            assert!(missing.is_empty(), "{lang:?} lacks {missing:#?}");
        }
    }
}
