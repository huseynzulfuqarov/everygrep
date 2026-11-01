//! The main window's commands and the keys that run them.
//!
//! [`COMMANDS`] gives every [`Command`] (declared in `ui/commands.slint`) a
//! stable id for the settings file, a category and label for the keyboard
//! settings and the help dialog, and its default keys. A [`Keymap`] holds the
//! keys in use: the defaults, with the commands the user changed replaced.
//!
//! A key is bound in a [`Scope`]: anywhere in the window, only in the search
//! field, or only in the result list. A binding of the focused control's
//! scope wins over a global one. In the search field, global bindings on the
//! keys that edit text are skipped, so the field keeps them.
//!
//! Nothing here touches a window, so all of it is tested headless.

use std::cell::RefCell;
use std::fmt;

use crate::i18n::tr;
use crate::{Command, MenuKeys};

/// Where a binding works.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Scope {
    /// Wherever the keyboard focus is in the main window.
    Global,
    /// Only while the search field has the focus.
    Search,
    /// Only while the result list has the focus.
    List,
}

impl Scope {
    pub const ALL: [Scope; 3] = [Scope::Global, Scope::Search, Scope::List];

    /// The word in the settings file.
    pub fn id(self) -> &'static str {
        match self {
            Scope::Global => "global",
            Scope::Search => "search",
            Scope::List => "list",
        }
    }

    pub fn from_id(id: &str) -> Option<Scope> {
        Scope::ALL.into_iter().find(|s| s.id() == id)
    }

    /// The scope numbered as the window passes it: 0 global, 1 search, 2 list.
    pub fn from_index(i: i32) -> Option<Scope> {
        usize::try_from(i)
            .ok()
            .and_then(|i| Scope::ALL.get(i).copied())
    }

    /// The scope's name after a key in a list: `Ctrl+C (Result list)`.
    pub fn name(self) -> &'static str {
        match self {
            Scope::Global => tr("Global"),
            Scope::Search => tr("Search field"),
            Scope::List => tr("Result list"),
        }
    }
}

/// Why a key cannot be bound in a scope, for the keyboard settings; `None`
/// when it can.
pub fn refusal(scope: Scope, chord: Chord) -> Option<&'static str> {
    let plain = !chord.ctrl && !chord.alt && !chord.meta;
    if matches!(chord.key, Key::Named("Esc")) || (plain && chord.key == Key::Named("Tab")) {
        return Some(tr(
            "Tab and Esc cannot be used: they move the focus and close dialogs.",
        ));
    }
    if plain && chord.key.printable() {
        return Some(tr(
            "Add Ctrl, Alt or Super: without them the key types into the search field.",
        ));
    }
    let movement = matches!(
        chord.key,
        Key::Named("Up" | "Down" | "PageUp" | "PageDown" | "Home" | "End")
    );
    if scope != Scope::Search && movement && !chord.alt && !chord.meta {
        return Some(tr(
            "Movement keys move in the result list and cannot be used here.",
        ));
    }
    if scope != Scope::List && chord.is_editing() {
        return Some(tr("The search field uses this key to edit text."));
    }
    let field_key = plain && !chord.shift && matches!(chord.key, Key::Named("Down" | "Enter"));
    if scope != Scope::List && field_key {
        return Some(tr("The search field uses this key."));
    }
    None
}

/// A key of a command with its scope, as listed in the keyboard settings.
pub fn binding_text(scope: Scope, chord: Chord) -> String {
    format!("{} ({})", chord.display(), scope.name())
}

/// Whether the keyboard settings list `spec` for the text typed in their
/// filter: a part of its category, its label or its keys, ignoring case and
/// diacritics.
pub fn matches_filter(filter: &str, spec: &Spec, keymap: &Keymap) -> bool {
    let fold = |text: &str| {
        everygrep_core::fold::strip_diacritics(&everygrep_core::fold::fold(text.as_bytes()))
    };
    let needle = fold(filter.trim());
    if needle.is_empty() {
        return true;
    }
    let keys: Vec<String> = keymap
        .of(spec.cmd)
        .flat_map(|b| [b.chord.display(), b.chord.to_string()])
        .collect();
    let hay = fold(&format!(
        "{} {} {}",
        spec.category.name(),
        (spec.label)(),
        keys.join(" ")
    ));
    hay.windows(needle.len()).any(|w| w == needle.as_slice())
}

/// The key of a [`Chord`], without its modifiers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Key {
    /// A key with a name (see [`NAMED`]).
    Named(&'static str),
    /// A key that types a character, in lower case.
    Char(char),
}

/// Keys with a name: the name in the settings file, the character the
/// toolkit gives for the key, and the text shown to the user. `+`, `,` and
/// `;` have names because they separate the parts of a settings line.
const NAMED: &[(&str, char, &str)] = &[
    ("Backspace", '\u{0008}', "Backspace"),
    ("Tab", '\u{0009}', "Tab"),
    ("Enter", '\u{000a}', "Enter"),
    ("Esc", '\u{001b}', "Esc"),
    ("Delete", '\u{007f}', "Delete"),
    ("Space", ' ', "Space"),
    ("Up", '\u{F700}', "↑"),
    ("Down", '\u{F701}', "↓"),
    ("Left", '\u{F702}', "←"),
    ("Right", '\u{F703}', "→"),
    ("F1", '\u{F704}', "F1"),
    ("F2", '\u{F705}', "F2"),
    ("F3", '\u{F706}', "F3"),
    ("F4", '\u{F707}', "F4"),
    ("F5", '\u{F708}', "F5"),
    ("F6", '\u{F709}', "F6"),
    ("F7", '\u{F70A}', "F7"),
    ("F8", '\u{F70B}', "F8"),
    ("F9", '\u{F70C}', "F9"),
    ("F10", '\u{F70D}', "F10"),
    ("F11", '\u{F70E}', "F11"),
    ("F12", '\u{F70F}', "F12"),
    ("F13", '\u{F710}', "F13"),
    ("F14", '\u{F711}', "F14"),
    ("F15", '\u{F712}', "F15"),
    ("F16", '\u{F713}', "F16"),
    ("F17", '\u{F714}', "F17"),
    ("F18", '\u{F715}', "F18"),
    ("F19", '\u{F716}', "F19"),
    ("F20", '\u{F717}', "F20"),
    ("F21", '\u{F718}', "F21"),
    ("F22", '\u{F719}', "F22"),
    ("F23", '\u{F71A}', "F23"),
    ("F24", '\u{F71B}', "F24"),
    ("Insert", '\u{F727}', "Insert"),
    ("Home", '\u{F729}', "Home"),
    ("End", '\u{F72B}', "End"),
    ("PageUp", '\u{F72C}', "PgUp"),
    ("PageDown", '\u{F72D}', "PgDn"),
    ("ScrollLock", '\u{F72F}', "ScrollLock"),
    ("Pause", '\u{F730}', "Pause"),
    ("Menu", '\u{F735}', "Menu"),
    ("Plus", '+', "+"),
    ("Comma", ',', ","),
    ("Semicolon", ';', ";"),
];

/// Characters the toolkit sends for a modifier key alone, and Caps Lock.
const MODIFIER_KEYS: &[char] = &[
    '\u{0010}', '\u{0011}', '\u{0012}', '\u{0013}', '\u{0014}', '\u{0015}', '\u{0016}', '\u{0017}',
    '\u{0018}',
];

/// Shift+Tab, as some backends send it.
const BACKTAB: char = '\u{0019}';

impl Key {
    /// The character the toolkit gives for the key.
    pub fn char(self) -> char {
        match self {
            Key::Named(name) => NAMED
                .iter()
                .find(|(n, ..)| *n == name)
                .map_or('\0', |(_, c, _)| *c),
            Key::Char(c) => c,
        }
    }

    fn of_char(c: char) -> Option<Key> {
        if MODIFIER_KEYS.contains(&c) {
            return None;
        }
        if let Some((name, ..)) = NAMED.iter().find(|(_, k, _)| *k == c) {
            return Some(Key::Named(name));
        }
        if c.is_control() {
            return None;
        }
        let mut lower = c.to_lowercase();
        match (lower.next(), lower.next()) {
            (Some(l), None) => Some(Key::Char(l)),
            _ => Some(Key::Char(c)),
        }
    }

    /// A key name from the settings file or a single character.
    fn parse(text: &str) -> Option<Key> {
        if let Some((name, ..)) = NAMED.iter().find(|(n, ..)| *n == text) {
            return Some(Key::Named(name));
        }
        let mut chars = text.chars();
        match (chars.next(), chars.next()) {
            (Some(c), None) => Key::of_char(c),
            _ => None,
        }
    }

    /// Whether the key types a character (a space included).
    fn printable(self) -> bool {
        matches!(
            self,
            Key::Char(_) | Key::Named("Space" | "Plus" | "Comma" | "Semicolon")
        )
    }

    /// The name in the settings file.
    fn name(self) -> String {
        match self {
            Key::Named(name) => name.to_owned(),
            Key::Char(c) => c.to_uppercase().collect(),
        }
    }

    /// The text shown to the user.
    fn display(self) -> String {
        match self {
            Key::Named(name) => NAMED
                .iter()
                .find(|(n, ..)| *n == name)
                .map_or(name, |(.., d)| *d)
                .to_owned(),
            Key::Char(c) => c.to_uppercase().collect(),
        }
    }
}

/// A key with its modifiers, such as `Ctrl+Shift+F5`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Chord {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    /// The Super (logo) key.
    pub meta: bool,
    pub key: Key,
}

impl Chord {
    /// The chord of a key event: its text and modifiers. `None` for a
    /// modifier pressed alone, an empty text or more than one character.
    pub fn from_event(text: &str, ctrl: bool, alt: bool, shift: bool, meta: bool) -> Option<Chord> {
        let mut chars = text.chars();
        let (Some(c), None) = (chars.next(), chars.next()) else {
            return None;
        };
        let (c, shift) = if c == BACKTAB {
            ('\t', true)
        } else {
            (c, shift)
        };
        Some(Chord {
            ctrl,
            alt,
            shift,
            meta,
            key: Key::of_char(c)?,
        })
    }

    /// Reads the text written by `Display`, such as `Ctrl+Alt+C`.
    pub fn parse(text: &str) -> Option<Chord> {
        let text = text.trim();
        let (mods, key) = match text.rsplit_once('+') {
            Some((mods, key)) => (mods, key),
            None => ("", text),
        };
        let mut chord = Chord {
            ctrl: false,
            alt: false,
            shift: false,
            meta: false,
            key: Key::parse(key)?,
        };
        for m in mods.split('+').filter(|m| !m.is_empty()) {
            let flag = match m {
                "Ctrl" => &mut chord.ctrl,
                "Alt" => &mut chord.alt,
                "Shift" => &mut chord.shift,
                "Super" => &mut chord.meta,
                _ => return None,
            };
            if *flag {
                return None;
            }
            *flag = true;
        }
        Some(chord)
    }

    fn modifiers(&self) -> String {
        let mut out = String::new();
        for (on, word) in [
            (self.ctrl, "Ctrl+"),
            (self.alt, "Alt+"),
            (self.shift, "Shift+"),
            (self.meta, "Super+"),
        ] {
            if on {
                out.push_str(word);
            }
        }
        out
    }

    /// The text shown to the user: `Ctrl+,`, `Alt+←`.
    pub fn display(&self) -> String {
        self.modifiers() + &self.key.display()
    }

    /// The toolkit's key value, for a menu item.
    pub fn to_keys(self) -> slint::Keys {
        let key = self.key.char().to_string();
        let parts = [
            (self.ctrl, "Control"),
            (self.alt, "Alt"),
            (self.shift, "Shift"),
            (self.meta, "Meta"),
        ];
        let parts = parts
            .into_iter()
            .filter_map(|(on, part)| on.then_some(part))
            .chain([key.as_str()]);
        slint::Keys::from_parts(parts).unwrap_or_default()
    }

    fn plain(self) -> bool {
        !self.ctrl && !self.alt && !self.meta
    }

    /// Whether the key edits text in the search field: typing, moving the
    /// text cursor, deleting, and Ctrl with A, C, V, X, Y or Z.
    pub fn is_editing(self) -> bool {
        if self.plain() && self.key.printable() {
            return true;
        }
        if self.alt || self.meta {
            return false;
        }
        match self.key {
            Key::Named("Left" | "Right" | "Home" | "End" | "Backspace" | "Delete") => true,
            Key::Char('a' | 'c' | 'v' | 'x' | 'y' | 'z') => self.ctrl,
            _ => false,
        }
    }
}

/// The settings file's text: modifiers in the order Ctrl, Alt, Shift,
/// Super, then the key's name (`Ctrl+Shift+F5`, `Alt+Left`, `Ctrl+Comma`).
impl fmt::Display for Chord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}{}", self.modifiers(), self.key.name())
    }
}

/// The group of a command, as in the menu bar.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Category {
    File,
    Edit,
    View,
    Search,
    Tools,
    Help,
}

impl Category {
    pub fn name(self) -> &'static str {
        match self {
            Category::File => tr("File"),
            Category::Edit => tr("Edit"),
            Category::View => tr("View"),
            Category::Search => tr("Search"),
            Category::Tools => tr("Tools"),
            Category::Help => tr("Help"),
        }
    }
}

/// One command of the table.
pub struct Spec {
    pub cmd: Command,
    /// Its id in the settings file; never changes once released.
    pub id: &'static str,
    pub category: Category,
    pub label: fn() -> &'static str,
    /// Default keys, as written by `Chord`'s `Display`.
    pub defaults: &'static [(Scope, &'static str)],
}

use Scope::{Global, List};

/// Every command, in the order of the help dialog and the keyboard settings.
pub const COMMANDS: &[Spec] = &[
    Spec {
        cmd: Command::NewWindow,
        id: "file.new-window",
        category: Category::File,
        label: || tr("New window"),
        defaults: &[(Global, "Ctrl+N")],
    },
    Spec {
        cmd: Command::CloseWindow,
        id: "file.close-window",
        category: Category::File,
        label: || tr("Close window"),
        defaults: &[(Global, "Ctrl+W")],
    },
    Spec {
        cmd: Command::ExportResults,
        id: "file.export-results",
        category: Category::File,
        label: || tr("Export results"),
        defaults: &[(Global, "Ctrl+S")],
    },
    Spec {
        cmd: Command::Quit,
        id: "file.quit",
        category: Category::File,
        label: || tr("Quit"),
        defaults: &[(Global, "Ctrl+Q")],
    },
    Spec {
        cmd: Command::Open,
        id: "file.open",
        category: Category::File,
        label: || tr("Open"),
        defaults: &[(List, "Enter")],
    },
    Spec {
        cmd: Command::OpenFolder,
        id: "file.open-folder",
        category: Category::File,
        label: || tr("Open containing folder"),
        defaults: &[(List, "Ctrl+Enter")],
    },
    Spec {
        cmd: Command::OpenWith,
        id: "file.open-with",
        category: Category::File,
        label: || tr("Open with…"),
        defaults: &[(List, "Shift+Enter")],
    },
    Spec {
        cmd: Command::Cut,
        id: "edit.cut",
        category: Category::Edit,
        label: || tr("Cut"),
        defaults: &[(List, "Ctrl+X")],
    },
    Spec {
        cmd: Command::Copy,
        id: "edit.copy",
        category: Category::Edit,
        label: || tr("Copy"),
        defaults: &[(List, "Ctrl+C")],
    },
    Spec {
        cmd: Command::Paste,
        id: "edit.paste",
        category: Category::Edit,
        label: || tr("Paste into folder"),
        defaults: &[(List, "Ctrl+V")],
    },
    Spec {
        cmd: Command::CopyName,
        id: "edit.copy-name",
        category: Category::Edit,
        label: || tr("Copy name"),
        defaults: &[(List, "Ctrl+Alt+C")],
    },
    Spec {
        cmd: Command::CopyPath,
        id: "edit.copy-path",
        category: Category::Edit,
        label: || tr("Copy path"),
        defaults: &[(List, "Ctrl+Alt+Shift+C")],
    },
    Spec {
        cmd: Command::CopyFullPath,
        id: "edit.copy-full-path",
        category: Category::Edit,
        label: || tr("Copy full path"),
        defaults: &[(List, "Ctrl+Shift+C")],
    },
    Spec {
        cmd: Command::SelectAll,
        id: "edit.select-all",
        category: Category::Edit,
        label: || tr("Select all"),
        defaults: &[(List, "Ctrl+A")],
    },
    Spec {
        cmd: Command::InvertSelection,
        id: "edit.invert-selection",
        category: Category::Edit,
        label: || tr("Invert selection"),
        defaults: &[(Global, "Ctrl+I")],
    },
    Spec {
        cmd: Command::ToggleRow,
        id: "edit.toggle-row",
        category: Category::Edit,
        label: || tr("Add or remove one row"),
        defaults: &[(List, "Ctrl+Space")],
    },
    Spec {
        cmd: Command::Rename,
        id: "edit.rename",
        category: Category::Edit,
        label: || tr("Rename"),
        defaults: &[(List, "F2")],
    },
    Spec {
        cmd: Command::Trash,
        id: "edit.trash",
        category: Category::Edit,
        label: || tr("Move to trash"),
        defaults: &[(List, "Delete")],
    },
    Spec {
        cmd: Command::DeletePermanently,
        id: "edit.delete-permanently",
        category: Category::Edit,
        label: || tr("Delete permanently"),
        defaults: &[(List, "Shift+Delete")],
    },
    Spec {
        cmd: Command::FocusSearch,
        id: "edit.focus-search",
        category: Category::Edit,
        label: || tr("Focus the search field"),
        defaults: &[(List, "Ctrl+F"), (List, "Ctrl+L")],
    },
    Spec {
        cmd: Command::ContextMenu,
        id: "edit.context-menu",
        category: Category::Edit,
        label: || tr("Context menu"),
        defaults: &[(List, "Menu"), (List, "Shift+F10")],
    },
    Spec {
        cmd: Command::TypeColumn,
        id: "view.type-column",
        category: Category::View,
        label: || tr("Type column"),
        defaults: &[],
    },
    Spec {
        cmd: Command::ExtensionColumn,
        id: "view.extension-column",
        category: Category::View,
        label: || tr("Extension column"),
        defaults: &[],
    },
    Spec {
        cmd: Command::CreatedColumn,
        id: "view.created-column",
        category: Category::View,
        label: || tr("Created column"),
        defaults: &[],
    },
    Spec {
        cmd: Command::AccessedColumn,
        id: "view.accessed-column",
        category: Category::View,
        label: || tr("Accessed column"),
        defaults: &[],
    },
    Spec {
        cmd: Command::AttributesColumn,
        id: "view.attributes-column",
        category: Category::View,
        label: || tr("Attributes column"),
        defaults: &[],
    },
    Spec {
        cmd: Command::SelectionDetails,
        id: "view.selection-details",
        category: Category::View,
        label: || tr("Selection details"),
        defaults: &[],
    },
    Spec {
        cmd: Command::MenuBar,
        id: "view.menu-bar",
        category: Category::View,
        label: || tr("Menu bar"),
        defaults: &[],
    },
    Spec {
        cmd: Command::MatchCase,
        id: "search.match-case",
        category: Category::Search,
        label: || tr("Match case"),
        defaults: &[(Global, "Alt+C")],
    },
    Spec {
        cmd: Command::WholeWords,
        id: "search.whole-words",
        category: Category::Search,
        label: || tr("Match whole words"),
        defaults: &[(Global, "Alt+W")],
    },
    Spec {
        cmd: Command::MatchPath,
        id: "search.match-path",
        category: Category::Search,
        label: || tr("Match the full path"),
        defaults: &[(Global, "Alt+P")],
    },
    Spec {
        cmd: Command::MatchDiacritics,
        id: "search.match-diacritics",
        category: Category::Search,
        label: || tr("Match diacritics"),
        defaults: &[(Global, "Alt+D")],
    },
    Spec {
        cmd: Command::Regex,
        id: "search.regex",
        category: Category::Search,
        label: || tr("Regular expression"),
        defaults: &[(Global, "Alt+R")],
    },
    Spec {
        cmd: Command::MatchPrefix,
        id: "search.match-prefix",
        category: Category::Search,
        label: || tr("Match prefix"),
        defaults: &[(Global, "Alt+B")],
    },
    Spec {
        cmd: Command::MatchSuffix,
        id: "search.match-suffix",
        category: Category::Search,
        label: || tr("Match suffix"),
        defaults: &[(Global, "Alt+E")],
    },
    Spec {
        cmd: Command::IgnorePunctuation,
        id: "search.ignore-punctuation",
        category: Category::Search,
        label: || tr("Ignore punctuation"),
        defaults: &[(Global, "Alt+U")],
    },
    Spec {
        cmd: Command::IgnoreWhitespace,
        id: "search.ignore-whitespace",
        category: Category::Search,
        label: || tr("Ignore whitespace"),
        defaults: &[(Global, "Alt+S")],
    },
    Spec {
        cmd: Command::PreviousFilter,
        id: "search.previous-filter",
        category: Category::Search,
        label: || tr("Previous filter"),
        defaults: &[(Global, "Alt+Up")],
    },
    Spec {
        cmd: Command::NextFilter,
        id: "search.next-filter",
        category: Category::Search,
        label: || tr("Next filter"),
        defaults: &[(Global, "Alt+Down")],
    },
    Spec {
        cmd: Command::PreviousSearch,
        id: "search.previous",
        category: Category::Search,
        label: || tr("Previous search"),
        defaults: &[(Global, "Alt+Left")],
    },
    Spec {
        cmd: Command::NextSearch,
        id: "search.next",
        category: Category::Search,
        label: || tr("Next search"),
        defaults: &[(Global, "Alt+Right")],
    },
    Spec {
        cmd: Command::Settings,
        id: "tools.settings",
        category: Category::Tools,
        label: || tr("Settings"),
        defaults: &[(Global, "Ctrl+Comma")],
    },
    Spec {
        cmd: Command::KeyboardSettings,
        id: "tools.keyboard-settings",
        category: Category::Tools,
        label: || tr("Customize shortcuts…"),
        defaults: &[],
    },
    Spec {
        cmd: Command::ShortcutsHelp,
        id: "help.shortcuts",
        category: Category::Help,
        label: || tr("Keyboard shortcuts"),
        defaults: &[],
    },
    Spec {
        cmd: Command::About,
        id: "help.about",
        category: Category::Help,
        label: || tr("About EveryGrep"),
        defaults: &[],
    },
];

/// The table entry of `cmd`.
pub fn spec(cmd: Command) -> Option<&'static Spec> {
    COMMANDS.iter().find(|s| s.cmd == cmd)
}

/// The table entry with the settings id `id`.
pub fn by_id(id: &str) -> Option<&'static Spec> {
    COMMANDS.iter().find(|s| s.id == id)
}

/// One key of a command.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Binding {
    pub cmd: Command,
    pub scope: Scope,
    pub chord: Chord,
}

/// The keys in use.
#[derive(Clone, Debug, PartialEq)]
pub struct Keymap {
    bindings: Vec<Binding>,
}

impl Default for Keymap {
    fn default() -> Self {
        Keymap::with_overrides(&[])
    }
}

impl Keymap {
    /// The default keys, with the keys of the commands in `overrides`
    /// (settings ids) replaced by the given ones; an empty list removes all
    /// keys of a command.
    pub fn with_overrides(overrides: &[(String, Vec<(Scope, Chord)>)]) -> Keymap {
        let mut bindings = Vec::new();
        for spec in COMMANDS {
            match overrides.iter().find(|(id, _)| id == spec.id) {
                Some((_, keys)) => {
                    for &(scope, chord) in keys {
                        bindings.push(Binding {
                            cmd: spec.cmd,
                            scope,
                            chord,
                        });
                    }
                }
                None => {
                    for &(scope, text) in spec.defaults {
                        let chord = Chord::parse(text).expect("default key parses");
                        bindings.push(Binding {
                            cmd: spec.cmd,
                            scope,
                            chord,
                        });
                    }
                }
            }
        }
        Keymap { bindings }
    }

    /// The commands whose keys differ from their defaults, with their keys,
    /// for the settings file.
    pub fn overrides(&self) -> Vec<(String, Vec<(Scope, Chord)>)> {
        let defaults = Keymap::with_overrides(&[]);
        COMMANDS
            .iter()
            .filter_map(|spec| {
                let keys = |k: &Keymap| -> Vec<(Scope, Chord)> {
                    k.of(spec.cmd).map(|b| (b.scope, b.chord)).collect()
                };
                let mine = keys(self);
                (mine != keys(&defaults)).then(|| (spec.id.to_owned(), mine))
            })
            .collect()
    }

    /// The other commands that have `chord` in `scope`; giving it to `cmd`
    /// takes it from them.
    pub fn conflicts(&self, cmd: Command, scope: Scope, chord: Chord) -> Vec<Command> {
        self.bindings
            .iter()
            .filter(|b| b.cmd != cmd && b.scope == scope && b.chord == chord)
            .map(|b| b.cmd)
            .collect()
    }

    /// Other commands with `chord` in an overlapping scope: a global key and
    /// the same key of the search field or the list. Both stay; the
    /// control's own key wins while it has the focus.
    pub fn also_used(&self, cmd: Command, scope: Scope, chord: Chord) -> Vec<(Scope, Command)> {
        self.bindings
            .iter()
            .filter(|b| {
                b.cmd != cmd
                    && b.chord == chord
                    && b.scope != scope
                    && (b.scope == Scope::Global || scope == Scope::Global)
            })
            .map(|b| (b.scope, b.cmd))
            .collect()
    }

    /// Gives `cmd` the key `chord` in `scope`, in place of its `replacing`th
    /// key or as a new one. Other commands with the same key in the same
    /// scope lose it; a key the command has already is not added twice.
    pub fn put(&mut self, cmd: Command, scope: Scope, chord: Chord, replacing: Option<usize>) {
        let edited = replacing.and_then(|n| {
            self.bindings
                .iter()
                .enumerate()
                .filter(|(_, b)| b.cmd == cmd)
                .nth(n)
                .map(|(i, _)| i)
        });
        let new = Binding { cmd, scope, chord };
        let twin = self.bindings.iter().position(|b| *b == new);
        match (edited, twin) {
            (Some(at), Some(t)) if at != t => {
                self.bindings.remove(at);
            }
            (Some(at), _) => self.bindings[at] = new,
            (None, Some(_)) => {}
            (None, None) => self.bindings.push(new),
        }
        self.bindings
            .retain(|b| b.cmd == cmd || b.scope != scope || b.chord != chord);
    }

    /// Removes the `n`th key of `cmd`; false when it has none such.
    pub fn remove(&mut self, cmd: Command, n: usize) -> bool {
        let Some(at) = self
            .bindings
            .iter()
            .enumerate()
            .filter(|(_, b)| b.cmd == cmd)
            .nth(n)
            .map(|(i, _)| i)
        else {
            return false;
        };
        self.bindings.remove(at);
        true
    }

    #[cfg(test)]
    pub fn bindings(&self) -> &[Binding] {
        &self.bindings
    }

    /// The bindings of `cmd`, in their order.
    pub fn of(&self, cmd: Command) -> impl Iterator<Item = &Binding> {
        self.bindings.iter().filter(move |b| b.cmd == cmd)
    }

    fn find(&self, scope: Scope, chord: &Chord) -> Option<Command> {
        self.bindings
            .iter()
            .find(|b| b.scope == scope && b.chord == *chord)
            .map(|b| b.cmd)
    }

    /// The command `chord` runs while the focus is in `scope`: a binding of
    /// that scope, else a global one. The search field keeps its text
    /// editing keys.
    pub fn resolve(&self, scope: Scope, chord: &Chord) -> Option<Command> {
        if scope == Scope::Global {
            return self.find(Scope::Global, chord);
        }
        self.find(scope, chord).or_else(|| {
            if scope == Scope::Search && chord.is_editing() {
                None
            } else {
                self.find(Scope::Global, chord)
            }
        })
    }

    /// The key shown next to `cmd` in the menu bar. The menu bar takes its
    /// keys before the focused control does, so only a global key that no
    /// control binds differently and that edits no text is given to it.
    pub fn menu_chord(&self, cmd: Command) -> Option<Chord> {
        self.of(cmd)
            .find(|b| {
                b.scope == Scope::Global
                    && !b.chord.is_editing()
                    && !self
                        .bindings
                        .iter()
                        .any(|o| o.scope != Scope::Global && o.chord == b.chord)
            })
            .map(|b| b.chord)
    }

    /// The keys of `cmd` for the user, `Menu, Shift+F10`.
    pub fn display(&self, cmd: Command) -> String {
        self.of(cmd)
            .map(|b| b.chord.display())
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// Lines of the help dialog: the keys that cannot be changed, then each
    /// command that has keys.
    pub fn help_rows(&self) -> Vec<(String, &'static str)> {
        let mut rows: Vec<(String, &'static str)> = vec![
            (
                "↑ ↓ PgUp PgDn Home End".to_owned(),
                tr("Move in the results and select"),
            ),
            (
                tr("Shift + movement").to_owned(),
                tr("Extend the selection"),
            ),
            (
                tr("Ctrl + movement").to_owned(),
                tr("Move without changing the selection"),
            ),
            ("Esc".to_owned(), tr("Clear the selection, then the search")),
        ];
        for spec in COMMANDS {
            let keys = self.display(spec.cmd);
            if !keys.is_empty() {
                rows.push((keys, (spec.label)()));
            }
        }
        rows
    }
}

/// Fills a `MenuKeys` field by field: `menu_keys!(keymap; field => Variant, ...)`.
macro_rules! menu_keys {
    ($k:expr; $($field:ident => $cmd:ident),* $(,)?) => {
        MenuKeys {
            $($field: $k.menu_chord(Command::$cmd).map(Chord::to_keys).unwrap_or_default(),)*
        }
    };
}

impl Keymap {
    /// The keys of the menu bar's items.
    pub fn menu_keys(&self) -> MenuKeys {
        menu_keys!(self;
            new_window => NewWindow,
            close_window => CloseWindow,
            export_results => ExportResults,
            quit => Quit,
            open => Open,
            open_folder => OpenFolder,
            open_with => OpenWith,
            cut => Cut,
            copy => Copy,
            paste => Paste,
            copy_name => CopyName,
            copy_path => CopyPath,
            copy_full_path => CopyFullPath,
            select_all => SelectAll,
            invert_selection => InvertSelection,
            toggle_row => ToggleRow,
            rename => Rename,
            trash => Trash,
            delete_permanently => DeletePermanently,
            focus_search => FocusSearch,
            context_menu => ContextMenu,
            type_column => TypeColumn,
            extension_column => ExtensionColumn,
            created_column => CreatedColumn,
            accessed_column => AccessedColumn,
            attributes_column => AttributesColumn,
            selection_details => SelectionDetails,
            menu_bar => MenuBar,
            match_case => MatchCase,
            whole_words => WholeWords,
            match_path => MatchPath,
            match_diacritics => MatchDiacritics,
            regex => Regex,
            match_prefix => MatchPrefix,
            match_suffix => MatchSuffix,
            ignore_punctuation => IgnorePunctuation,
            ignore_whitespace => IgnoreWhitespace,
            previous_filter => PreviousFilter,
            next_filter => NextFilter,
            previous_search => PreviousSearch,
            next_search => NextSearch,
            settings => Settings,
            keyboard_settings => KeyboardSettings,
            shortcuts_help => ShortcutsHelp,
            about => About,
        )
    }

    /// A command's label with its keys, `Settings (Ctrl+,)`, or the label
    /// alone without keys.
    pub fn labelled(&self, cmd: Command) -> String {
        let label = spec(cmd).map_or("", |s| (s.label)());
        match self.display(cmd) {
            keys if keys.is_empty() => label.to_owned(),
            keys => format!("{label} ({keys})"),
        }
    }
}

thread_local! {
    /// The keys of the running window.
    static CURRENT: RefCell<Keymap> = RefCell::default();
}

/// Uses `keymap` from now on.
pub fn set_current(keymap: Keymap) {
    CURRENT.with(|k| *k.borrow_mut() = keymap);
}

/// Runs `f` with the keys in use.
pub fn with_current<R>(f: impl FnOnce(&Keymap) -> R) -> R {
    CURRENT.with(|k| f(&k.borrow()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chord(text: &str) -> Chord {
        Chord::parse(text).unwrap_or_else(|| panic!("{text} does not parse"))
    }

    /// The commands of the table, in order.
    fn all_commands() -> Vec<Command> {
        COMMANDS.iter().map(|s| s.cmd).collect()
    }

    #[test]
    fn ids_and_commands_are_unique() {
        let mut ids: Vec<&str> = COMMANDS.iter().map(|s| s.id).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), COMMANDS.len());
        let cmds = all_commands();
        for (i, c) in cmds.iter().enumerate() {
            assert!(!cmds[i + 1..].contains(c), "{c:?} twice");
            assert_ne!(*c, Command::None);
        }
        for spec in COMMANDS {
            assert!(
                spec.id.contains('.')
                    && spec
                        .id
                        .chars()
                        .all(|c| c.is_ascii_lowercase() || c == '.' || c == '-'),
                "{}",
                spec.id
            );
            assert!(!(spec.label)().is_empty());
        }
    }

    #[test]
    fn every_command_of_the_window_is_in_the_table() {
        // The variants as written in ui/commands.slint.
        let source = include_str!("../ui/commands.slint");
        let body = &source[source.find('{').unwrap() + 1..source.find('}').unwrap()];
        let names: Vec<&str> = body
            .split(',')
            .map(|v| v.trim())
            .filter(|v| !v.is_empty() && *v != "none")
            .collect();
        assert_eq!(names.len(), COMMANDS.len(), "{names:?}");
        for (name, spec) in names.iter().zip(COMMANDS) {
            let camel: String = name
                .split('-')
                .map(|w| {
                    let mut c = w.chars();
                    c.next()
                        .unwrap()
                        .to_uppercase()
                        .chain(c)
                        .collect::<String>()
                })
                .collect();
            assert_eq!(
                camel,
                format!("{:?}", spec.cmd),
                "table order follows the enum"
            );
        }
    }

    #[test]
    fn defaults_parse_and_never_clash() {
        let map = Keymap::default();
        let b = map.bindings();
        assert!(b.len() >= 30);
        for (i, x) in b.iter().enumerate() {
            for y in &b[i + 1..] {
                assert!(
                    !(x.chord == y.chord && x.scope == y.scope),
                    "{:?} and {:?} share {}",
                    x.cmd,
                    y.cmd,
                    x.chord
                );
            }
        }
    }

    #[test]
    fn chord_text_round_trips() {
        for text in [
            "Ctrl+N",
            "Ctrl+Comma",
            "Ctrl+Alt+Shift+C",
            "Alt+Left",
            "Shift+F10",
            "Menu",
            "Ctrl+Space",
            "Ctrl+Shift+F5",
            "Ctrl+Super+Plus",
            "Ctrl+Semicolon",
            "Alt+Ə",
            "Ctrl+!",
            "Enter",
        ] {
            assert_eq!(chord(text).to_string(), text);
            assert_eq!(Chord::parse(&chord(text).to_string()), Some(chord(text)));
        }
        assert_eq!(chord("Ctrl+c"), chord("Ctrl+C"));
        assert_eq!(chord("Ctrl+,"), chord("Ctrl+Comma"));
        for bad in [
            "",
            "Ctrl+",
            "Hyper+A",
            "Ctrl+Ctrl+A",
            "Ctrl+AB",
            "Ctrl+Nope",
        ] {
            assert_eq!(Chord::parse(bad), None, "{bad}");
        }
        assert_eq!(chord("Ctrl+Comma").display(), "Ctrl+,");
        assert_eq!(chord("Alt+Left").display(), "Alt+←");
        assert_eq!(chord("Ctrl+Enter").display(), "Ctrl+Enter");
    }

    #[test]
    fn events_become_chords() {
        // Shift+C gives the capital letter; Caps Lock gives it without Shift.
        assert_eq!(
            Chord::from_event("C", true, false, true, false),
            Some(chord("Ctrl+Shift+C"))
        );
        assert_eq!(
            Chord::from_event("C", true, false, false, false),
            Some(chord("Ctrl+C"))
        );
        assert_eq!(
            Chord::from_event("\u{F708}", false, false, false, false),
            Some(chord("F5"))
        );
        assert_eq!(
            Chord::from_event("\n", true, false, false, false),
            Some(chord("Ctrl+Enter"))
        );
        assert_eq!(
            Chord::from_event("\u{0019}", false, false, false, false),
            Some(chord("Shift+Tab"))
        );
        // A modifier alone, nothing, or text of several characters.
        for text in [
            "\u{0010}", "\u{0011}", "\u{0012}", "\u{0014}", "\u{0017}", "", "ab",
        ] {
            assert_eq!(
                Chord::from_event(text, true, false, false, false),
                None,
                "{text:?}"
            );
        }
    }

    #[test]
    fn menu_keys_are_built() {
        let k = chord("Ctrl+Comma").to_keys();
        assert_eq!(k.to_parts().collect::<Vec<_>>(), ["Control", ","]);
        let k = chord("Alt+Left").to_keys();
        assert_eq!(k.to_parts().collect::<Vec<_>>(), ["Alt", "\u{F702}"]);
        let k = chord("Ctrl+Shift+C").to_keys();
        assert_eq!(k.to_parts().collect::<Vec<_>>(), ["Control", "Shift", "c"]);
    }

    /// Every key of the window before the keys could be changed.
    #[test]
    fn the_old_keys_still_work() {
        use Command as C;
        let map = Keymap::default();
        let both = [Scope::Search, Scope::List, Scope::Global];
        let list = [Scope::List];
        let table: &[(&[Scope], &str, C)] = &[
            (&list, "Ctrl+Space", C::ToggleRow),
            (&list, "Ctrl+A", C::SelectAll),
            (&both, "Ctrl+I", C::InvertSelection),
            (&list, "Enter", C::Open),
            (&list, "Ctrl+Enter", C::OpenFolder),
            (&list, "Shift+Enter", C::OpenWith),
            (&list, "Ctrl+X", C::Cut),
            (&list, "Ctrl+C", C::Copy),
            (&list, "Ctrl+V", C::Paste),
            (&list, "Ctrl+Shift+C", C::CopyFullPath),
            (&list, "Ctrl+Alt+C", C::CopyName),
            (&list, "Ctrl+Alt+Shift+C", C::CopyPath),
            (&list, "F2", C::Rename),
            (&list, "Delete", C::Trash),
            (&list, "Shift+Delete", C::DeletePermanently),
            (&list, "Menu", C::ContextMenu),
            (&list, "Shift+F10", C::ContextMenu),
            (&list, "Ctrl+F", C::FocusSearch),
            (&list, "Ctrl+L", C::FocusSearch),
            (&both, "Alt+Left", C::PreviousSearch),
            (&both, "Alt+Right", C::NextSearch),
            (&both, "Alt+C", C::MatchCase),
            (&both, "Alt+W", C::WholeWords),
            (&both, "Alt+P", C::MatchPath),
            (&both, "Alt+D", C::MatchDiacritics),
            (&both, "Alt+R", C::Regex),
            (&both, "Alt+B", C::MatchPrefix),
            (&both, "Alt+E", C::MatchSuffix),
            (&both, "Alt+U", C::IgnorePunctuation),
            (&both, "Alt+S", C::IgnoreWhitespace),
            (&both, "Alt+Up", C::PreviousFilter),
            (&both, "Alt+Down", C::NextFilter),
            (&both, "Ctrl+S", C::ExportResults),
            (&both, "Ctrl+N", C::NewWindow),
            (&both, "Ctrl+Comma", C::Settings),
            (&both, "Ctrl+Q", C::Quit),
            (&both, "Ctrl+W", C::CloseWindow),
        ];
        for (scopes, text, cmd) in table {
            for &scope in *scopes {
                assert_eq!(
                    map.resolve(scope, &chord(text)),
                    Some(*cmd),
                    "{text} in {scope:?}"
                );
            }
        }
        // List keys do nothing elsewhere.
        assert_eq!(map.resolve(Scope::Search, &chord("F2")), None);
        assert_eq!(map.resolve(Scope::Global, &chord("Ctrl+C")), None);
    }

    #[test]
    fn the_search_field_keeps_its_editing_keys() {
        // A global binding on an editing key, as an old settings file might
        // have: the list uses it, the search field does not.
        let map = Keymap::with_overrides(&[(
            "edit.select-all".to_owned(),
            vec![(Scope::Global, chord("Ctrl+A"))],
        )]);
        assert_eq!(
            map.resolve(Scope::List, &chord("Ctrl+A")),
            Some(Command::SelectAll)
        );
        assert_eq!(map.resolve(Scope::Search, &chord("Ctrl+A")), None);
        for text in [
            "Ctrl+A",
            "Ctrl+C",
            "Ctrl+V",
            "Ctrl+X",
            "Ctrl+Z",
            "Ctrl+Y",
            "Ctrl+Left",
            "Ctrl+Shift+Right",
            "Home",
            "Shift+End",
            "Backspace",
            "Delete",
            "A",
            "Shift+A",
            "Space",
            "Comma",
        ] {
            assert!(chord(text).is_editing(), "{text}");
        }
        for text in [
            "Ctrl+N",
            "Alt+A",
            "F2",
            "Ctrl+Space",
            "Alt+Left",
            "Ctrl+Comma",
        ] {
            assert!(!chord(text).is_editing(), "{text}");
        }
    }

    #[test]
    fn menu_keys_leave_scoped_and_editing_keys_to_the_controls() {
        let map = Keymap::default();
        assert_eq!(map.menu_chord(Command::NewWindow), Some(chord("Ctrl+N")));
        assert_eq!(map.menu_chord(Command::Settings), Some(chord("Ctrl+Comma")));
        assert_eq!(map.menu_chord(Command::MatchCase), Some(chord("Alt+C")));
        // List keys would be taken from the search field.
        assert_eq!(map.menu_chord(Command::Copy), None);
        assert_eq!(map.menu_chord(Command::Rename), None);
        assert_eq!(map.menu_chord(Command::TypeColumn), None);
        // A global key that the list binds differently stays off the menu.
        let map = Keymap::with_overrides(&[
            ("edit.copy".to_owned(), vec![(Scope::List, chord("Ctrl+N"))]),
            (
                "edit.select-all".to_owned(),
                vec![(Scope::Global, chord("Ctrl+A"))],
            ),
            (
                "view.menu-bar".to_owned(),
                vec![(Scope::Global, chord("Ctrl+M"))],
            ),
        ]);
        assert_eq!(map.menu_chord(Command::NewWindow), None);
        assert_eq!(
            map.resolve(Scope::Search, &chord("Ctrl+N")),
            Some(Command::NewWindow)
        );
        assert_eq!(
            map.resolve(Scope::List, &chord("Ctrl+N")),
            Some(Command::Copy)
        );
        assert_eq!(map.menu_chord(Command::SelectAll), None, "an editing key");
        assert_eq!(map.menu_chord(Command::MenuBar), Some(chord("Ctrl+M")));
        let keys = map.menu_keys();
        assert_eq!(keys.menu_bar, chord("Ctrl+M").to_keys());
        assert_eq!(keys.new_window, slint::Keys::default());
        assert_eq!(map.labelled(Command::MenuBar), "Menu bar (Ctrl+M)");
        assert_eq!(map.labelled(Command::About), "About EveryGrep");
    }

    #[test]
    fn menu_keys_have_a_field_per_command() {
        let source = include_str!("../ui/commands.slint");
        let fields = &source[source.find("struct MenuKeys").unwrap()..];
        let enum_body = &source[source.find('{').unwrap() + 1..source.find('}').unwrap()];
        let variants: Vec<&str> = enum_body
            .split(',')
            .map(str::trim)
            .filter(|v| !v.is_empty() && *v != "none")
            .collect();
        for v in &variants {
            assert!(fields.contains(&format!("    {v}: keys,")), "{v}");
        }
        assert_eq!(fields.matches(": keys,").count(), variants.len());
    }

    #[test]
    fn the_filter_finds_labels_categories_and_keys() {
        let map = Keymap::default();
        let found = |filter: &str| -> Vec<&str> {
            COMMANDS
                .iter()
                .filter(|s| matches_filter(filter, s, &map))
                .map(|s| s.id)
                .collect()
        };
        assert_eq!(found("").len(), COMMANDS.len());
        assert_eq!(found("  ").len(), COMMANDS.len());
        assert!(found("copy full").contains(&"edit.copy-full-path"));
        assert!(found("COPY FULL").contains(&"edit.copy-full-path"));
        assert_eq!(found("ctrl+shift+c"), ["edit.copy-full-path"]);
        assert_eq!(found("F2"), ["edit.rename"]);
        assert!(found("tools").contains(&"tools.settings"));
        assert!(found("diacritics").contains(&"search.match-diacritics"));
        // Diacritics in the filter are ignored too.
        assert!(found("dïacritics").contains(&"search.match-diacritics"));
        // The arrow and the name both find a key.
        assert!(found("alt+←").contains(&"search.previous"));
        assert!(found("alt+left").contains(&"search.previous"));
        assert!(found("nothing like this").is_empty());
    }

    #[test]
    fn keys_are_removed_one_at_a_time() {
        let mut map = Keymap::default();
        assert!(map.remove(Command::FocusSearch, 1));
        assert_eq!(map.display(Command::FocusSearch), "Ctrl+F");
        assert!(!map.remove(Command::FocusSearch, 1));
        assert_eq!(
            map.overrides(),
            [(
                "edit.focus-search".to_owned(),
                vec![(Scope::List, chord("Ctrl+F"))]
            )]
        );
        assert!(map.remove(Command::FocusSearch, 0));
        assert_eq!(map.overrides(), [("edit.focus-search".to_owned(), vec![])]);
        assert_eq!(map.resolve(Scope::List, &chord("Ctrl+F")), None);
        assert_eq!(
            binding_text(Scope::List, chord("Ctrl+C")),
            "Ctrl+C (Result list)"
        );
    }

    #[test]
    fn adding_a_key_takes_it_from_others_in_the_same_scope_only() {
        let mut map = Keymap::default();
        let f9 = chord("Ctrl+N");
        assert_eq!(
            map.conflicts(Command::Copy, Scope::Global, f9),
            [Command::NewWindow]
        );
        assert!(map.conflicts(Command::Copy, Scope::List, f9).is_empty());
        assert_eq!(
            map.also_used(Command::Copy, Scope::List, f9),
            [(Scope::Global, Command::NewWindow)]
        );
        assert_eq!(
            map.also_used(Command::Quit, Scope::Global, chord("Ctrl+C")),
            [(Scope::List, Command::Copy)]
        );
        assert!(
            map.also_used(Command::Quit, Scope::Search, chord("Ctrl+C"))
                .is_empty()
        );
        // A list key next to a global one: both stay.
        map.put(Command::Copy, Scope::List, f9, None);
        assert_eq!(map.display(Command::NewWindow), "Ctrl+N");
        assert_eq!(map.resolve(Scope::List, &f9), Some(Command::Copy));
        assert_eq!(map.resolve(Scope::Search, &f9), Some(Command::NewWindow));
        // The same scope: it moves.
        map.put(Command::Quit, Scope::Global, f9, None);
        assert_eq!(map.display(Command::NewWindow), "");
        assert_eq!(map.display(Command::Quit), "Ctrl+Q, Ctrl+N");
        // Twice the same key is one key.
        map.put(Command::Quit, Scope::Global, f9, None);
        assert_eq!(map.display(Command::Quit), "Ctrl+Q, Ctrl+N");
        // Editing a key replaces it; editing it into a key it has removes it.
        map.put(Command::Quit, Scope::Global, chord("Ctrl+Shift+Q"), Some(0));
        assert_eq!(map.display(Command::Quit), "Ctrl+Shift+Q, Ctrl+N");
        map.put(Command::Quit, Scope::Global, f9, Some(0));
        assert_eq!(map.display(Command::Quit), "Ctrl+N");
    }

    #[test]
    fn keys_that_would_break_typing_or_moving_are_refused() {
        use Scope::{Global as G, List as L, Search as S};
        let refused = |scope, text: &str| refusal(scope, chord(text)).is_some();
        for scope in [G, S, L] {
            for text in [
                "Tab",
                "Shift+Tab",
                "Esc",
                "Ctrl+Esc",
                "A",
                "Shift+A",
                "Space",
                "Comma",
                "Ə",
            ] {
                assert!(refused(scope, text), "{text} in {scope:?}");
            }
            for text in [
                "Ctrl+Tab",
                "Ctrl+Shift+F5",
                "Alt+A",
                "Super+E",
                "F5",
                "Ctrl+N",
            ] {
                assert!(!refused(scope, text), "{text} in {scope:?}");
            }
        }
        for text in ["Down", "Shift+PageUp", "Ctrl+Home", "End"] {
            assert!(refused(L, text) && refused(G, text), "{text}");
        }
        assert!(!refused(L, "Alt+Down"));
        for text in [
            "Ctrl+A",
            "Ctrl+V",
            "Ctrl+Left",
            "Backspace",
            "Delete",
            "Down",
            "Enter",
        ] {
            assert!(refused(S, text) && refused(G, text), "{text}");
        }
        assert!(!refused(L, "Ctrl+A") && !refused(L, "Delete") && !refused(L, "Enter"));
        // Every default key is allowed where it is bound.
        for b in Keymap::default().bindings() {
            assert_eq!(refusal(b.scope, b.chord), None, "{} {:?}", b.chord, b.scope);
        }
    }

    #[test]
    fn help_rows_cover_every_bound_command() {
        let map = Keymap::default();
        let rows = map.help_rows();
        for spec in COMMANDS {
            if map.of(spec.cmd).next().is_some() {
                assert!(
                    rows.iter().any(|(_, a)| *a == (spec.label)()),
                    "{}",
                    spec.id
                );
            }
        }
        assert!(rows.contains(&("Menu, Shift+F10".to_owned(), "Context menu")));
        assert!(rows.contains(&("Ctrl+F, Ctrl+L".to_owned(), "Focus the search field")));
    }
}
