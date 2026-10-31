//! Which entries are left out of the index: excluded folders, and filters
//! for the names of files to include or exclude.
//!
//! The rules come from three lists, and a switch that turns all of them off:
//!
//! - **Excluded folders.** Each item is an absolute folder path, matched
//!   exactly, or a pattern. A folder that matches is not indexed at all:
//!   neither its own entry nor anything below it.
//! - **Include only files.** When the list is not empty, a file (any entry
//!   that is not a folder) is indexed only if at least one item matches it.
//! - **Exclude files.** A file that matches any item is not indexed.
//!
//! A file is indexed when (the include list is empty or one of its items
//! matches) and no exclude item matches; there is no order between items.
//! Folders are never tested against the file lists.
//!
//! The file lists are written as one text, items separated by `;` or `,`;
//! an item in double quotes keeps its separators, and `""` inside quotes is
//! one quote. An item is a wildcard pattern, or a regular expression after
//! `regex:`:
//!
//! - `*` matches any run of characters except `/`, `**` any run including
//!   `/`, and `?` one character except `/`.
//! - A wildcard pattern is matched against the whole name, ignoring case. An
//!   item with a `/` (or `**`) is matched against the full path instead: one
//!   that starts with `/` against the whole path, any other against the end
//!   of the path after a `/` (`docs/*.txt` matches `/home/u/docs/a.txt`).
//! - `regex:` items are searched for in the name (in the full path when the
//!   expression contains a `/`), with the case as written; `(?i)` ignores
//!   it.
//!
//! The rules are compiled once ([`IndexRules::new`]); an item that does not
//! compile is an error, so a broken list is never used. The roots of the
//! index are never tested: they are indexed because they were asked for.

use std::collections::HashSet;
use std::fmt;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use regex::bytes::{Regex, RegexBuilder};

/// Prefix of an item that is a regular expression.
pub const REGEX_PREFIX: &str = "regex:";

/// Largest compiled size of one regular expression, as for queries.
const REGEX_SIZE_LIMIT: usize = 10 * (1 << 20);

/// The rules as the user wrote them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuleSpec {
    /// Whether the lists are used at all; off leaves nothing out.
    pub enabled: bool,
    /// Excluded folders: absolute paths or patterns, one per item.
    pub folders: Vec<PathBuf>,
    /// Files to include, as one `;`-separated text; empty includes all.
    pub include_files: String,
    /// Files to exclude, as one `;`-separated text.
    pub exclude_files: String,
}

impl Default for RuleSpec {
    fn default() -> Self {
        Self {
            enabled: true,
            folders: Vec::new(),
            include_files: String::new(),
            exclude_files: String::new(),
        }
    }
}

/// Which list a broken item is in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RuleList {
    Folders,
    IncludeFiles,
    ExcludeFiles,
}

/// An item that could not be compiled.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuleError {
    pub list: RuleList,
    /// The item as written.
    pub item: String,
    /// Why, from the regular expression compiler; empty for an empty
    /// expression.
    pub reason: String,
}

impl fmt::Display for RuleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.reason.is_empty() {
            write!(f, "\"{}\" has an empty regular expression", self.item)
        } else {
            write!(f, "\"{}\" is not valid: {}", self.item, self.reason)
        }
    }
}

impl std::error::Error for RuleError {}

/// How one item matches a name or a path.
#[derive(Debug)]
enum Matcher {
    /// Matches any text (`*`, `**`).
    Any,
    /// The whole text, ASCII case ignored.
    Exact(Vec<u8>),
    Prefix(Vec<u8>),
    Suffix(Vec<u8>),
    Contains(Vec<u8>),
    Regex(Regex),
}

impl Matcher {
    fn matches(&self, text: &[u8]) -> bool {
        match self {
            Matcher::Any => true,
            Matcher::Exact(p) => text.eq_ignore_ascii_case(p),
            Matcher::Prefix(p) => text.len() >= p.len() && text[..p.len()].eq_ignore_ascii_case(p),
            Matcher::Suffix(p) => {
                text.len() >= p.len() && text[text.len() - p.len()..].eq_ignore_ascii_case(p)
            }
            Matcher::Contains(p) => text.windows(p.len()).any(|w| w.eq_ignore_ascii_case(p)),
            Matcher::Regex(r) => r.is_match(text),
        }
    }
}

#[derive(Debug)]
struct Rule {
    matcher: Matcher,
    /// Matched against the full path rather than the name.
    full_path: bool,
}

#[derive(Debug, Default)]
struct Compiled {
    /// Folder paths matched exactly, as bytes.
    folder_paths: HashSet<Vec<u8>>,
    folders: Vec<Rule>,
    include: Vec<Rule>,
    exclude: Vec<Rule>,
    /// Whether any folder or file rule needs the full path.
    folder_paths_needed: bool,
    file_paths_needed: bool,
}

/// Compiled rules, cheap to clone. `Default` leaves nothing out.
#[derive(Clone, Default)]
pub struct IndexRules {
    spec: Option<RuleSpec>,
    compiled: Arc<Compiled>,
}

impl fmt::Debug for IndexRules {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("IndexRules").field(&self.spec()).finish()
    }
}

impl PartialEq for IndexRules {
    fn eq(&self, other: &Self) -> bool {
        self.spec() == other.spec()
    }
}

impl IndexRules {
    /// Compiles `spec`. Fails on the first item that is not a valid
    /// pattern.
    pub fn new(spec: RuleSpec) -> Result<Self, RuleError> {
        let mut c = Compiled::default();
        if spec.enabled {
            for folder in &spec.folders {
                let bytes = folder.as_os_str().as_bytes();
                let text = String::from_utf8_lossy(bytes);
                let item = text.trim();
                if item.is_empty() {
                    continue;
                }
                if item.starts_with('/') && !item.contains(['*', '?']) {
                    let mut path = bytes.to_vec();
                    while path.len() > 1 && path.ends_with(b"/") {
                        path.pop();
                    }
                    c.folder_paths.insert(path);
                } else {
                    c.folders.push(compile(item, RuleList::Folders)?);
                }
            }
            for item in split_items(&spec.include_files) {
                c.include.push(compile(&item, RuleList::IncludeFiles)?);
            }
            for item in split_items(&spec.exclude_files) {
                c.exclude.push(compile(&item, RuleList::ExcludeFiles)?);
            }
        }
        c.folder_paths_needed = !c.folder_paths.is_empty() || c.folders.iter().any(|r| r.full_path);
        c.file_paths_needed = c.include.iter().chain(&c.exclude).any(|r| r.full_path);
        Ok(Self {
            spec: Some(spec),
            compiled: Arc::new(c),
        })
    }

    /// The rules as written; the defaults for [`IndexRules::default`].
    pub fn spec(&self) -> RuleSpec {
        self.spec.clone().unwrap_or_default()
    }

    /// Whether the rules leave nothing out.
    pub fn is_empty(&self) -> bool {
        let c = &self.compiled;
        c.folder_paths.is_empty()
            && c.folders.is_empty()
            && c.include.is_empty()
            && c.exclude.is_empty()
    }

    /// Whether a rule depends on more than an entry's own name, so moving a
    /// folder can change what is left out below it.
    pub fn uses_paths(&self) -> bool {
        let c = &self.compiled;
        c.folder_paths_needed || c.file_paths_needed
    }

    /// Folders excluded by their exact path.
    pub fn excluded_paths(&self) -> HashSet<PathBuf> {
        self.compiled
            .folder_paths
            .iter()
            .map(|p| PathBuf::from(std::ffi::OsStr::from_bytes(p)))
            .collect()
    }

    /// Whether the entry `name` in the folder `parent` is indexed.
    pub fn accepts(&self, parent: &Path, name: &[u8], is_dir: bool) -> bool {
        let c = &*self.compiled;
        let (rules, needs_path): (&[Rule], bool) = if is_dir {
            if c.folder_paths.is_empty() && c.folders.is_empty() {
                return true;
            }
            (&c.folders, c.folder_paths_needed)
        } else {
            if c.include.is_empty() && c.exclude.is_empty() {
                return true;
            }
            (&[], c.file_paths_needed)
        };
        let path = needs_path.then(|| {
            let parent = parent.as_os_str().as_bytes();
            let mut p = Vec::with_capacity(parent.len() + 1 + name.len());
            p.extend_from_slice(parent);
            if !parent.ends_with(b"/") {
                p.push(b'/');
            }
            p.extend_from_slice(name);
            p
        });
        let hit = |r: &Rule| match (&path, r.full_path) {
            (Some(p), true) => r.matcher.matches(p),
            _ => r.matcher.matches(name),
        };
        if is_dir {
            if path.as_ref().is_some_and(|p| c.folder_paths.contains(p)) {
                return false;
            }
            return !rules.iter().any(hit);
        }
        (c.include.is_empty() || c.include.iter().any(hit)) && !c.exclude.iter().any(hit)
    }

    /// Like [`accepts`](Self::accepts) for an entry given by its full path.
    pub fn accepts_path(&self, path: &Path, is_dir: bool) -> bool {
        match (path.parent(), path.file_name()) {
            (Some(parent), Some(name)) => self.accepts(parent, name.as_bytes(), is_dir),
            _ => true,
        }
    }
}

/// Splits a list of items at `;` and `,`, keeping separators inside double
/// quotes; `""` inside quotes is a quote. Items are trimmed; empty ones are
/// dropped.
pub fn split_items(text: &str) -> Vec<String> {
    let mut items = Vec::new();
    let mut item = String::new();
    let mut quoted = false;
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '"' if quoted && chars.peek() == Some(&'"') => {
                chars.next();
                item.push('"');
            }
            '"' => quoted = !quoted,
            ';' | ',' if !quoted => items.push(std::mem::take(&mut item)),
            _ => item.push(ch),
        }
    }
    items.push(item);
    items
        .into_iter()
        .map(|i| i.trim().to_owned())
        .filter(|i| !i.is_empty())
        .collect()
}

fn compile(item: &str, list: RuleList) -> Result<Rule, RuleError> {
    let error = |reason: String| RuleError {
        list,
        item: item.to_owned(),
        reason,
    };
    if let Some(expr) = item.strip_prefix(REGEX_PREFIX) {
        if expr.is_empty() {
            return Err(error(String::new()));
        }
        let regex = RegexBuilder::new(expr)
            .size_limit(REGEX_SIZE_LIMIT)
            .build()
            .map_err(|e| error(e.to_string()))?;
        return Ok(Rule {
            matcher: Matcher::Regex(regex),
            full_path: expr.contains('/'),
        });
    }
    let mut pattern = item;
    while pattern.len() > 1 && pattern.ends_with('/') {
        pattern = &pattern[..pattern.len() - 1];
    }
    let full_path = pattern.contains('/') || pattern.contains("**");
    if pattern.chars().all(|c| c == '*') {
        return Ok(Rule {
            matcher: Matcher::Any,
            full_path: false,
        });
    }
    if !full_path && pattern.is_ascii() && !pattern.contains('?') {
        let inner = pattern.trim_matches('*');
        if !inner.contains('*') {
            let lead = pattern.starts_with('*');
            let trail = pattern.ends_with('*');
            let bytes = inner.as_bytes().to_vec();
            let matcher = match (lead, trail) {
                (false, false) => Matcher::Exact(bytes),
                (true, false) => Matcher::Suffix(bytes),
                (false, true) => Matcher::Prefix(bytes),
                (true, true) => Matcher::Contains(bytes),
            };
            return Ok(Rule { matcher, full_path });
        }
    }
    let mut expr = String::from("(?i)");
    if full_path && !pattern.starts_with('/') {
        expr.push_str("(?:^|/)");
    } else {
        expr.push('^');
    }
    let mut rest = pattern;
    while let Some(ch) = rest.chars().next() {
        if let Some(after) = rest.strip_prefix("**") {
            expr.push_str("(?s-u:.)*");
            rest = after;
            continue;
        }
        match ch {
            '*' => expr.push_str("(?-u:[^/])*"),
            '?' => expr.push_str("[^/]"),
            c => expr.push_str(&regex::escape(c.encode_utf8(&mut [0; 4]))),
        }
        rest = &rest[ch.len_utf8()..];
    }
    expr.push('$');
    let regex = RegexBuilder::new(&expr)
        .size_limit(REGEX_SIZE_LIMIT)
        .build()
        .map_err(|e| error(e.to_string()))?;
    Ok(Rule {
        matcher: Matcher::Regex(regex),
        full_path,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules(folders: &[&str], include: &str, exclude: &str) -> IndexRules {
        IndexRules::new(RuleSpec {
            enabled: true,
            folders: folders.iter().map(PathBuf::from).collect(),
            include_files: include.into(),
            exclude_files: exclude.into(),
        })
        .unwrap()
    }

    fn file(r: &IndexRules, path: &str) -> bool {
        r.accepts_path(Path::new(path), false)
    }

    fn dir(r: &IndexRules, path: &str) -> bool {
        r.accepts_path(Path::new(path), true)
    }

    #[test]
    fn splits_on_semicolons_and_commas_with_quotes() {
        assert_eq!(split_items(" *.mp3; *.jpg ,,x "), ["*.mp3", "*.jpg", "x"]);
        assert_eq!(
            split_items(r#""a;b","say ""hi""";c"#),
            ["a;b", r#"say "hi""#, "c"]
        );
        assert!(split_items(" ; , ").is_empty());
    }

    #[test]
    fn nothing_is_left_out_by_default() {
        let r = IndexRules::default();
        assert!(file(&r, "/a/b.tmp") && dir(&r, "/a/b"));
        assert!(!r.uses_paths() && r.is_empty());
        assert_eq!(r.spec(), RuleSpec::default());
    }

    #[test]
    fn folders_by_exact_path_and_by_pattern() {
        let r = rules(
            &["/home/u/.cache/", "node_modules", "*.git", "/srv/**/tmp"],
            "",
            "",
        );
        assert!(!dir(&r, "/home/u/.cache"));
        assert!(dir(&r, "/home/u/.cache2"));
        assert!(dir(&r, "/home/U/.cache"), "paths are exact");
        assert!(!dir(&r, "/x/NODE_MODULES"), "names ignore case");
        assert!(!dir(&r, "/x/repo.git"));
        assert!(!dir(&r, "/srv/a/b/tmp"));
        assert!(dir(&r, "/home/tmp"));
        // Folder rules never touch files.
        assert!(file(&r, "/x/node_modules"));
        assert!(file(&r, "/home/u/.cache"));
    }

    #[test]
    fn file_wildcards_match_the_whole_name_or_the_path() {
        let r = rules(&[], "", "*.TMP;~*;*cache*;a?c;docs/*.txt;/etc/*.conf");
        assert!(!file(&r, "/x/a.tmp"));
        assert!(file(&r, "/x/a.tmp.keep"), "the whole name is matched");
        assert!(!file(&r, "/x/~lock"));
        assert!(!file(&r, "/x/My Cache File"));
        assert!(!file(&r, "/x/abc") && file(&r, "/x/abbc"));
        assert!(!file(&r, "/home/u/docs/a.txt"));
        assert!(
            file(&r, "/home/u/mydocs/a.txt"),
            "a path item starts after a /"
        );
        assert!(file(&r, "/home/u/docs/sub/a.txt"), "* does not cross /");
        assert!(!file(&r, "/etc/x.conf") && file(&r, "/etc/a/x.conf"));
        assert!(r.uses_paths());
        // Folders are not tested against file lists.
        assert!(dir(&r, "/x/a.tmp"));
    }

    #[test]
    fn non_ascii_and_odd_bytes() {
        let r = rules(&[], "", "*.ŞƏK;Ç*");
        assert!(!file(&r, "/x/a.şək"));
        assert!(!file(&r, "/x/çay"));
        let raw = IndexRules::new(RuleSpec {
            exclude_files: "*.bin".into(),
            ..RuleSpec::default()
        })
        .unwrap();
        assert!(!raw.accepts(Path::new("/x"), b"\xff\xfe.bin", false));
        let star = rules(&[], "", "a*z");
        assert!(!star.accepts(Path::new("/x"), b"a\xffz", false));
    }

    #[test]
    fn regex_items() {
        let r = rules(&["regex:^te?mp$"], "", r"regex:\.(bak|old)$;regex:/build/");
        assert!(!dir(&r, "/x/tmp") && !dir(&r, "/x/temp"));
        assert!(dir(&r, "/x/TMP"), "case as written");
        assert!(!file(&r, "/x/a.bak") && file(&r, "/x/a.BAK"));
        assert!(!file(&r, "/p/build/out.o"));
        assert!(file(&r, "/p/rebuild.o"));
        let ci = rules(&[], "", r"regex:(?i)\.bak$");
        assert!(!file(&ci, "/x/A.BAK"));
    }

    #[test]
    fn include_only_and_exclude_together() {
        let r = rules(&[], "*.mp3;*.jpg", "*secret*");
        assert!(file(&r, "/m/song.mp3") && file(&r, "/m/p.JPG"));
        assert!(!file(&r, "/m/notes.txt"), "not in the include list");
        assert!(
            !file(&r, "/m/secret.mp3"),
            "an exclude wins over an include"
        );
        assert!(
            dir(&r, "/m/folder.txt"),
            "folders are not filtered by files"
        );
    }

    #[test]
    fn switched_off_leaves_nothing_out() {
        let r = IndexRules::new(RuleSpec {
            enabled: false,
            folders: vec!["/a".into(), "x*".into()],
            include_files: "*.mp3".into(),
            exclude_files: "*".into(),
        })
        .unwrap();
        assert!(dir(&r, "/a") && dir(&r, "/b/xy") && file(&r, "/b/c.txt"));
        assert!(r.excluded_paths().is_empty());
        assert!(!r.spec().enabled, "the lists are kept");
    }

    #[test]
    fn broken_items_are_errors() {
        for (list, spec) in [
            (
                RuleList::ExcludeFiles,
                RuleSpec {
                    exclude_files: "*.ok;regex:(open".into(),
                    ..RuleSpec::default()
                },
            ),
            (
                RuleList::IncludeFiles,
                RuleSpec {
                    include_files: "regex:".into(),
                    ..RuleSpec::default()
                },
            ),
            (
                RuleList::Folders,
                RuleSpec {
                    folders: vec!["regex:[z-a]".into()],
                    ..RuleSpec::default()
                },
            ),
        ] {
            let e = IndexRules::new(spec).unwrap_err();
            assert_eq!(e.list, list);
            assert!(e.item.starts_with("regex:"), "{e}");
            assert!(!e.to_string().is_empty());
        }
        // Text that looks like regex syntax is literal in a wildcard.
        let r = rules(&[], "", "a(b[c");
        assert!(!file(&r, "/x/a(b[c"));
    }
}
