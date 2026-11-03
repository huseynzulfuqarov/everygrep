//! The texts of the help window (F1): the search syntax, regular
//! expressions and the command line options.
//!
//! Each page is a list of rows: an example as it is typed (never
//! translated) and what it does, with headings between the groups. The
//! full reference is `docs/query-language.md`; this is the short form for
//! the window. Tests check that every example parses and that the command
//! line page lists exactly the options the program accepts.

use crate::i18n::tr;

/// One line of a help page.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    /// What is typed; empty for a heading.
    pub example: &'static str,
    /// What it does, or the heading's text.
    pub meaning: &'static str,
}

fn heading(text: &'static str) -> Row {
    Row {
        example: "",
        meaning: text,
    }
}

fn row(example: &'static str, meaning: &'static str) -> Row {
    Row { example, meaning }
}

/// The search syntax page.
pub fn syntax() -> Vec<Row> {
    vec![
        heading(tr("Words and operators")),
        row("report pdf", tr("Both words: a space means AND")),
        row("jpg|png", tr("Either word: | means OR")),
        row("!draft", tr("Names without the word: ! means NOT")),
        row("<draft|final> pdf", tr("Groups with < and >")),
        row(
            "\"two words\"",
            tr("Quotes make spaces and operators plain text"),
        ),
        heading(tr("Wildcards")),
        row(
            "*.pdf",
            tr("* is any run of characters; the pattern must match the whole name"),
        ),
        row("img_??.png", tr("? is exactly one character")),
        row(
            "path:/home/me/**.txt",
            tr("In a path * stays inside one folder, ** crosses folders"),
        ),
        heading(tr("Modifiers, written before a word")),
        row("case:Report", tr("Match case")),
        row("wholeword:art", tr("Whole words only")),
        row(
            "path:docs/report",
            tr("Match the full path (a word with / does this too)"),
        ),
        row("prefix:rep", tr("The name starts with the word")),
        row("suffix:.tar.gz", tr("The name ends with the word")),
        row("wholename:main.rs", tr("The whole name is the word")),
        row(
            "diacritics:café",
            tr("Accents count: cafe no longer finds café"),
        ),
        row(
            "nopunct:foo-bar",
            tr("Ignore punctuation: also finds foo_bar and foobar"),
        ),
        row("nospace:myfile", tr("Ignore spaces: also finds my file")),
        row(
            "regex:^IMG_\\d+",
            tr("A regular expression, see the next page"),
        ),
        heading(tr("Kinds, sizes and dates")),
        row("file:report", tr("Files only")),
        row("folder:src", tr("Folders only")),
        row("ext:pdf;jpg", tr("Files with one of these extensions")),
        row(
            "image: beach",
            tr("A kind: audio:, archive:, code:, document:, executable:, image:, video:"),
        ),
        row("size:>10mb", tr("Larger than 10 MB; also <, <=, >=")),
        row("size:1kb..2mb", tr("A size between two values")),
        row(
            "size:large",
            tr("Size words: empty, tiny, small, medium, large, huge, gigantic"),
        ),
        row(
            "modified:today",
            tr("Modified today; also yesterday, thisweek, lastmonth, 2026-10"),
        ),
        row("modified:<7d", tr("Modified in the last 7 days")),
        row(
            "created:thisyear",
            tr("Created this year (read from disk while searching)"),
        ),
        row(
            "accessed:>30d",
            tr("Not opened for 30 days (read from disk while searching)"),
        ),
        heading(tr("Contents and results")),
        row(
            "content:invoice ext:txt",
            tr(
                "Files whose contents hold the text (read from disk; add other words to keep it fast)",
            ),
        ),
        row(
            "duplicates: ext:jpg",
            tr("Files whose contents another result has too"),
        ),
        row(
            "limit:10",
            tr("Only the first 10 results in the shown order"),
        ),
    ]
}

/// The regular expression page.
pub fn regex() -> Vec<Row> {
    vec![
        heading(tr("Where expressions are used")),
        row(
            "regex:^IMG_\\d+",
            tr("One word of the query is an expression, up to the next space"),
        ),
        row(
            "Alt+R",
            tr("Regex mode: the whole search text is one expression"),
        ),
        heading(tr("Characters")),
        row(".", tr("Any one character")),
        row("[abc]", tr("One of the listed characters")),
        row("[^abc]", tr("Any character but the listed ones")),
        row("[a-z]", tr("A character in the range")),
        row("\\d", tr("A digit")),
        row("\\w", tr("A letter, digit or underscore")),
        row("\\s", tr("A space or another whitespace character")),
        row(
            "\\.",
            tr("A dot itself: a backslash makes a special character plain"),
        ),
        heading(tr("Repeats")),
        row("a*", tr("Zero or more times")),
        row("a+", tr("One or more times")),
        row("a?", tr("Zero or one time")),
        row("a{2,4}", tr("Two to four times")),
        heading(tr("Positions and groups")),
        row("^report", tr("At the start of the name")),
        row("\\.pdf$", tr("At the end of the name")),
        row("\\bart\\b", tr("A whole word")),
        row(
            "(jpe?g|png)$",
            tr("Either alternative, grouped with ( and )"),
        ),
        row("(?-i)README", tr("Match case inside the expression")),
        heading(tr("Notes")),
        row(
            "regex:^se",
            tr("Accents are not ignored: this does not find Şəkil"),
        ),
        row(
            "regex:\"^my file\"",
            tr("Quote an expression that has spaces"),
        ),
    ]
}

/// The command line options page.
pub fn command_line() -> Vec<Row> {
    vec![
        heading(tr("everygrep-gui [options] [query words…]")),
        row("--root <path>", tr("A folder to index; repeat it for more")),
        row("--index-file <path>", tr("Where the index is kept")),
        row(
            "--filter <name>",
            tr(
                "Start with a filter: all, audio, archive, code, document, executable, folder, image, video",
            ),
        ),
        row("--case", tr("Match case")),
        row("--whole-words", tr("Match whole words only")),
        row(
            "--match-path",
            tr("Match the full path instead of the name"),
        ),
        row("--diacritics", tr("Match diacritics")),
        row("--prefix", tr("Words must match at the start of the name")),
        row("--suffix", tr("Words must match at the end of the name")),
        row("--ignore-punctuation", tr("Ignore punctuation")),
        row("--ignore-whitespace", tr("Ignore whitespace")),
        row("--regex", tr("The whole query is one regular expression")),
        row(
            "--columns <list>",
            tr("Show columns: type, extension, created, accessed, attributes"),
        ),
        row(
            "--new-instance",
            tr("Open another window even when one is open"),
        ),
        row(
            "--toggle",
            tr("Show the window, or close it when it is in front"),
        ),
        row(
            "--new-window",
            tr("Open another window over the same index"),
        ),
        row(
            "--background",
            tr("Start without a window when closing only hides it"),
        ),
        row(
            "--settings[=<page>]",
            tr(
                "Open Settings too, at a page: indexing, search, appearance, window, keyboard, advanced",
            ),
        ),
        row("-h, --help", tr("Print the options and quit")),
        row("-V, --version", tr("Print the version and quit")),
        row("--", tr("Every argument after it is a query word")),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use everygrep_core::query::{Query, QueryOptions};

    #[test]
    fn every_syntax_example_parses() {
        for r in syntax().iter().filter(|r| !r.example.is_empty()) {
            if let Err(e) = Query::parse(r.example, QueryOptions::default()) {
                panic!("{}: {e:?}", r.example);
            }
        }
    }

    #[test]
    fn every_expression_example_parses() {
        let whole = QueryOptions {
            regex: true,
            ..QueryOptions::default()
        };
        for r in regex().iter().filter(|r| !r.example.is_empty()) {
            let parsed = if r.example.starts_with("regex:") {
                Query::parse(r.example, QueryOptions::default())
            } else if r.example == "Alt+R" {
                continue;
            } else {
                Query::parse(r.example, whole)
            };
            if let Err(e) = parsed {
                panic!("{}: {e:?}", r.example);
            }
        }
    }

    #[test]
    fn the_command_line_page_lists_every_option() {
        let usage = crate::USAGE;
        let listed: Vec<&str> = command_line()
            .iter()
            .flat_map(|r| r.example.split(", "))
            .filter_map(|e| e.split([' ', '[']).next())
            .filter(|o| o.starts_with('-'))
            .collect();
        for option in &listed {
            if *option != "--" {
                assert!(
                    usage.contains(&format!(" {option}")),
                    "{option} is not an option"
                );
            }
        }
        let accepted: Vec<&str> = usage
            .lines()
            .map(str::trim_start)
            .filter(|l| l.starts_with('-'))
            .flat_map(|l| l.split("  ").next().unwrap().split(", "))
            .map(|o| o.split([' ', '[']).next().unwrap())
            .collect();
        assert!(accepted.len() >= 20, "{accepted:?}");
        for option in accepted {
            assert!(
                listed.contains(&option),
                "{option} is missing from the help"
            );
        }
    }

    #[test]
    fn pages_start_with_a_heading() {
        for page in [syntax(), regex(), command_line()] {
            assert!(page[0].example.is_empty());
            assert!(page.iter().all(|r| !r.meaning.is_empty()));
        }
    }
}
