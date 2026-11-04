//! RFC 4180 CSV records shared by the CSV imports and exports: quoted
//! commas, doubled quotes and embedded line breaks, CRLF or LF between
//! records, and protection of spreadsheet formula starters.

/// Where a CSV text is malformed: the 1-based line the bad record starts on.
#[derive(Debug, PartialEq, Eq)]
pub struct Malformed {
    pub line: usize,
}

/// Appends one field, quoted when it holds a comma, quote or line break.
pub fn field(out: &mut String, text: &str) {
    if text.contains([',', '"', '\r', '\n']) {
        out.push('"');
        out.push_str(&text.replace('"', "\"\""));
        out.push('"');
    } else {
        out.push_str(text);
    }
}

/// Appends a record of fields and the CRLF that ends it.
pub fn record<'a>(out: &mut String, fields: impl IntoIterator<Item = &'a str>) {
    for (i, f) in fields.into_iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        field(out, f);
    }
    out.push_str("\r\n");
}

/// A spreadsheet runs a field starting with `=`, `+`, `-` or `@` as a
/// formula; a leading apostrophe keeps it text. An existing leading
/// apostrophe is doubled, so [`unprotect`] restores the exact text.
pub fn protect(text: &str) -> String {
    if text.starts_with(['=', '+', '-', '@', '\'']) {
        format!("'{text}")
    } else {
        text.into()
    }
}

pub fn unprotect(text: &str) -> &str {
    text.strip_prefix('\'')
        .filter(|s| s.starts_with(['=', '+', '-', '@', '\'']))
        .unwrap_or(text)
}

/// Strict CSV state machine. Quotes inside an unquoted field, junk after a
/// closing quote, a lone CR and an unclosed quote fail. A UTF-8 byte order
/// mark at the start is skipped.
pub fn rows(text: &str) -> Result<Vec<Vec<String>>, Malformed> {
    #[derive(Clone, Copy, PartialEq)]
    enum State {
        Start,
        Plain,
        Quoted,
        Closed,
    }
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut state = State::Start;
    let mut rows = Vec::new();
    let mut row = Vec::new();
    let mut value = String::new();
    let mut chars = text.chars().peekable();
    let mut pending = false;
    let mut line = 1;
    let mut record_line = 1;
    while let Some(c) = chars.next() {
        if !pending {
            record_line = line;
        }
        pending = true;
        if c == '\n' {
            line += 1;
        }
        if state == State::Quoted {
            if c == '"' {
                if chars.peek() == Some(&'"') {
                    chars.next();
                    value.push('"');
                } else {
                    state = State::Closed;
                }
            } else {
                value.push(c);
            }
            continue;
        }
        let bad = Malformed { line: record_line };
        match c {
            '"' if state == State::Start => state = State::Quoted,
            '"' => return Err(bad),
            ',' => {
                row.push(std::mem::take(&mut value));
                state = State::Start;
            }
            '\r' | '\n' => {
                if c == '\r' {
                    if chars.next() != Some('\n') {
                        return Err(bad);
                    }
                    line += 1;
                }
                row.push(std::mem::take(&mut value));
                rows.push(std::mem::take(&mut row));
                state = State::Start;
                pending = false;
            }
            _ if state == State::Closed => return Err(bad),
            _ => {
                value.push(c);
                state = State::Plain;
            }
        }
    }
    if state == State::Quoted {
        return Err(Malformed { line: record_line });
    }
    if pending {
        row.push(value);
        rows.push(row);
    }
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fields_round_trip_quotes_commas_and_line_breaks() {
        let values = ["plain", "a,b", "say \"hi\"", "two\nlines", "cr\r\nlf", ""];
        let mut text = String::new();
        record(&mut text, values);
        record(&mut text, ["x"]);
        let rows = rows(&text).unwrap();
        assert_eq!(rows[0], values);
        assert_eq!(rows[1], ["x"]);
    }

    #[test]
    fn malformed_text_names_the_record_line() {
        assert_eq!(rows("a,b\nc\"d\n"), Err(Malformed { line: 2 }));
        assert_eq!(rows("a\n\"open\n\nx"), Err(Malformed { line: 2 }));
        assert_eq!(rows("\"a\"b\n"), Err(Malformed { line: 1 }));
        assert_eq!(rows("a\rb"), Err(Malformed { line: 1 }));
        assert_eq!(rows("\u{feff}a,b").unwrap(), [["a", "b"]]);
    }

    #[test]
    fn formula_starters_are_protected_and_restored() {
        for text in ["=1+1", "-ext:tmp", "'quoted", "@x", "+y", "normal"] {
            assert_eq!(unprotect(&protect(text)), text);
        }
        assert_eq!(protect("=1"), "'=1");
        assert_eq!(unprotect("'plain"), "'plain");
    }
}
