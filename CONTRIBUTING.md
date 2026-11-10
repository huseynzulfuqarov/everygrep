# Contributing

## Building and testing

```sh
cargo build                     # both programs, debug
cargo run -p everygrep-gui      # the window
cargo run -p everygrep-cli -- search 'ext:pdf'
./scripts/check.sh              # the gate every commit must pass
```

`scripts/check.sh` runs `cargo fmt --all --check`, `cargo clippy --workspace
--all-targets --locked -- -D warnings` and `cargo test --workspace --locked`.

The single-window test needs a private session bus and is skipped by default:

```sh
EVERYGREP_PRIVATE_BUS=1 dbus-run-session -- cargo test -p everygrep-gui -- --ignored
```

To try the window without touching your own index and settings, give it
its own folders, index file and configuration folder:

```sh
XDG_CONFIG_HOME=/tmp/eg-config cargo run -p everygrep-gui -- \
    --root ~/some/folder --index-file /tmp/eg-index.bin
```

`scripts/smoke-gui.sh [binary] [seconds] [rss-limit-MiB]` starts a built
window the same way in a scratch folder, leaves it idle and fails if its UI
thread stays busy or its memory passes the limit (default
`target/release/everygrep-gui`, 20 s, 600 MiB). It needs a desktop session,
so `scripts/check.sh` does not run it.

## Commits

- [Conventional Commits](https://www.conventionalcommits.org/) in English:
  `feat(gui): …`, `fix(core): …`, `docs: …`, `chore: …`, `build: …`,
  `perf(core): …`, `refactor: …`. Scopes are `core`, `cli`, `gui`, `query`,
  `data`.
- Small, logical commits; every commit passes `scripts/check.sh` on its own.
- A user-visible change updates `docs/manual.md` (and the README or the
  other files in `docs/` where it applies) in the same or the next commit,
  and gets a line in `CHANGELOG.md`.
- The docs describe what the code does; check each claim against the code or
  a test.

## Dependencies

`everygrep-core` depends on `regex` (regular expressions in queries),
`rustix` (opening files and folders without following links), and
`grep-searcher`, `grep-regex` and `encoding_rs` (reading file contents for
`content:`), `md-5`, `sha1`, `sha2`, `crc32fast` and `twox-hash` (hashes of
file contents), `lofty` and `kamadak-exif` (audio and photo metadata), and
`rawzip`, `flate2` and `quick-xml` (Office and EPUB documents); keep that list
short. The window uses Slint (with its winit backend crate pinned, for keys
by position), zbus (session bus), `x11rb` (files on the XWayland clipboard),
`wayland-client` and `wayland-protocols` (keeping dialogs above the window
that opened them) and `rustix`. **Do not add a crate, or a feature that pulls in new
crates, without discussing it first**; `git diff Cargo.lock` should normally
be empty. When the dependencies do change, regenerate the license list:

```sh
python3 -I scripts/third-party-licenses.py
```

and check its output for licenses that are not permissive.

## UI

Read [docs/design.md](docs/design.md) before changing the window, and
[docs/icons.md](docs/icons.md) before adding icons (all icons are original
drawings). Every action needs a key, and colors come from the tokens in
`ui/theme.slint`.

## Translations

The window's texts are written in English in the code and translated through
gettext catalogs in
`crates/everygrep-gui/translations/<lang>/LC_MESSAGES/everygrep-gui.po`.

- In `.slint` files, mark a text with `@tr("…")`; placeholders are `{}`
  (`@tr("Version {}", root.version)`).
- In Rust, use `i18n::tr("…")` for a fixed text and
  `i18n::trn("one …", "many …", n)` for a text with a count, then
  `i18n::fill(text, &[&arg, …])` for the placeholders (`{}` in order, or
  `{0}`, `{1}` by position). The text must be a string literal so the tests
  can find it. Texts on standard error and the command line stay English.
- Add the new text to every catalog with its translation. The test
  `i18n::tests::every_text_is_translated` fails when a marked text is
  missing from a catalog, when a catalog entry is no longer used, or when the
  placeholders differ.

Plural forms: every catalog uses `Plural-Forms: nplurals=2; plural=(n != 1);`.
Azerbaijani and Turkish keep the noun singular after a number ("12 fayl",
"12 dosya"), so both forms are usually the same; the first form is used for
exactly 1 and can be worded apart ("Ad kopyalandı." for one name).

To add a language:

1. Copy one of the catalogs to
   `translations/<code>/LC_MESSAGES/everygrep-gui.po`, set `Language:` and
   `Plural-Forms:` in its header, and translate every `msgstr`.
2. Add the language to `Lang` in `crates/everygrep-gui/src/i18n.rs` (its code,
   its own name, its position in `CATALOGS` and `catalog_index`) and to
   `LanguageChoice::ALL` in `settings.rs`.
3. If its plural rule is not `n != 1`, teach `trn_in` the rule.
4. Translate `GenericName`, `Comment` and `Keywords` in
   `data/az.huseynz.EveryGrep.desktop`.

`msgfmt --check -o /dev/null <file>` (from gettext) catches syntax errors.
Use the terms of the desktop for that language (folder, trash, settings) and
keep texts short; check the result in the window, since some places have
little room (column headers, buttons, the status line).

## License

By contributing you agree that your work is licensed under MIT OR Apache-2.0,
as described in the README.
