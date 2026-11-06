//! Shortcuts by key position (Settings → Keyboard, on by default): a
//! shortcut with Ctrl, Alt or Super is taken by the key pressed, whatever
//! the keyboard layout, so `Ctrl+W` stays on the same key with an
//! Azerbaijani, Turkish or Russian layout. The toolkit's key event carries
//! only the text a key types, so the key itself is read from the window's
//! raw keyboard events just before ([`watch`]) and named by the character
//! it types on a US layout ([`us_char`]). Keys without a character (F1,
//! arrows) and keys typed without those modifiers are taken as they come.

use std::cell::Cell;

use i_slint_backend_winit::winit::event::{ElementState, WindowEvent};
use i_slint_backend_winit::winit::keyboard::{Key, KeyCode, PhysicalKey};
use i_slint_backend_winit::winit::platform::modifier_supplement::KeyEventExtModifierSupplement;
use i_slint_backend_winit::{EventResult, WinitWindowAccessor};

use crate::commands::Chord;

thread_local! {
    /// The US-layout character of the key pressed last, if it has one.
    static LAST: Cell<Option<char>> = const { Cell::new(None) };
    // The same key without Shift, in the active layout (never a US guess).
    static LAYOUT: Cell<Option<char>> = const { Cell::new(None) };
    static ENABLED: Cell<bool> = const { Cell::new(true) };
}

/// Turns matching by key position on or off.
pub fn set_enabled(on: bool) {
    ENABLED.with(|e| e.set(on));
}

/// Notes every key pressed in `window` before the toolkit handles it.
pub fn watch(window: &slint::Window) {
    window.on_winit_window_event(|_, event| {
        if let WindowEvent::KeyboardInput { event, .. } = event
            && event.state == ElementState::Pressed
        {
            let c = match event.physical_key {
                PhysicalKey::Code(code) => us_char(code),
                PhysicalKey::Unidentified(_) => None,
            };
            LAST.with(|l| l.set(c));
            let layout = match event.key_without_modifiers() {
                Key::Character(text) => {
                    let mut chars = text.chars();
                    chars.next().filter(|_| chars.next().is_none())
                }
                _ => None,
            };
            LAYOUT.with(|l| l.set(layout));
        }
        EventResult::Propagate
    });
}

/// The character a key types on a US layout, for keys that type one.
pub fn us_char(code: KeyCode) -> Option<char> {
    use KeyCode::*;
    let letters = [
        KeyA, KeyB, KeyC, KeyD, KeyE, KeyF, KeyG, KeyH, KeyI, KeyJ, KeyK, KeyL, KeyM, KeyN, KeyO,
        KeyP, KeyQ, KeyR, KeyS, KeyT, KeyU, KeyV, KeyW, KeyX, KeyY, KeyZ,
    ];
    if let Some(i) = letters.iter().position(|k| *k == code) {
        return char::from_u32('a' as u32 + i as u32);
    }
    let digits = [
        Digit0, Digit1, Digit2, Digit3, Digit4, Digit5, Digit6, Digit7, Digit8, Digit9,
    ];
    if let Some(i) = digits.iter().position(|k| *k == code) {
        return char::from_digit(i as u32, 10);
    }
    Some(match code {
        Minus | NumpadSubtract => '-',
        Equal => '=',
        NumpadAdd => '+',
        BracketLeft => '[',
        BracketRight => ']',
        Backslash => '\\',
        Semicolon => ';',
        Quote => '\'',
        Comma => ',',
        Period => '.',
        Slash => '/',
        Backquote => '`',
        _ => return None,
    })
}

/// The chord of a key event: by key position when that is on, the key has
/// a character and Ctrl, Alt or Super is held; otherwise by its text.
pub fn chord(text: &str, ctrl: bool, alt: bool, shift: bool, meta: bool) -> Option<Chord> {
    pick(
        text,
        (ctrl, alt, shift, meta),
        LAST.with(Cell::get),
        ENABLED.with(Cell::get),
    )
}

/// [`chord`] with the key pressed and the setting given.
fn pick(
    text: &str,
    (ctrl, alt, shift, meta): (bool, bool, bool, bool),
    key: Option<char>,
    enabled: bool,
) -> Option<Chord> {
    let by_text = Chord::from_event(text, ctrl, alt, shift, meta);
    match key {
        // A named key (Enter, F5) is the same on every layout.
        Some(c)
            if enabled && (ctrl || alt || meta) && by_text.is_some_and(|t| t.key.printable()) =>
        {
            Chord::from_event(&c.to_string(), ctrl, alt, shift, meta).or(by_text)
        }
        _ => by_text,
    }
}

/// The chords to look a key event up by, in order: [`chord`], then the
/// chord of its text when that differs, so a key bound by its character
/// (before matching by position, or one only a layout has) still works.
pub fn chords(text: &str, ctrl: bool, alt: bool, shift: bool, meta: bool) -> Vec<Chord> {
    candidates(
        text,
        (ctrl, alt, shift, meta),
        LAST.with(Cell::get),
        LAYOUT.with(Cell::get),
        ENABLED.with(Cell::get),
    )
}

fn candidates(
    text: &str,
    (ctrl, alt, shift, meta): (bool, bool, bool, bool),
    key: Option<char>,
    layout: Option<char>,
    enabled: bool,
) -> Vec<Chord> {
    let mut out: Vec<Chord> = pick(text, (ctrl, alt, shift, meta), key, enabled)
        .into_iter()
        .collect();
    let mut add = |chord| {
        if let Some(chord) = chord
            && !out.contains(&chord)
        {
            out.push(chord);
        }
    };
    add(Chord::from_event(text, ctrl, alt, shift, meta));
    // Keep explicit character bindings first, then accept the unshifted
    // active-layout key with Shift still held: ')' can match Shift+0.
    if shift
        && (ctrl || alt || meta)
        && let Some(c) = layout
    {
        add(Chord::from_event(&c.to_string(), ctrl, alt, shift, meta));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const CTRL: (bool, bool, bool, bool) = (true, false, false, false);

    #[test]
    fn shifted_zoom_keys_work_by_active_layout_without_position_matching() {
        use crate::Command;
        use crate::commands::{Keymap, Scope};
        let map = Keymap::default();
        for (text, layout, expected) in
            [(")", '0', Command::ZoomReset), ("+", '=', Command::ZoomIn)]
        {
            let keys = candidates(
                text,
                (true, false, true, false),
                Some(layout),
                Some(layout),
                false,
            );
            assert_eq!(
                keys.iter().find_map(|c| map.resolve(Scope::Global, c)),
                Some(expected)
            );
        }
        // Layout matching never maps an Azerbaijani letter to its US position.
        let keys = candidates("Ü", (true, false, true, false), Some('w'), Some('ü'), false);
        assert!(keys.contains(&Chord::parse("Ctrl+Shift+Ü").unwrap()));
        assert!(!keys.contains(&Chord::parse("Ctrl+Shift+W").unwrap()));
        // An explicit binding on the typed symbol retains priority.
        let keys = candidates("+", (true, false, true, false), Some('='), Some('='), false);
        assert_eq!(keys[0], Chord::parse("Ctrl+Shift+Plus").unwrap());
        // Plain typing is never translated to an unshifted character.
        assert_eq!(
            candidates(
                ")",
                (false, false, true, false),
                Some('0'),
                Some('0'),
                false
            )
            .len(),
            1
        );
    }

    #[test]
    fn keys_are_named_as_on_a_us_layout() {
        assert_eq!(us_char(KeyCode::KeyA), Some('a'));
        assert_eq!(us_char(KeyCode::KeyZ), Some('z'));
        assert_eq!(us_char(KeyCode::Digit0), Some('0'));
        assert_eq!(us_char(KeyCode::Digit9), Some('9'));
        assert_eq!(us_char(KeyCode::Equal), Some('='));
        assert_eq!(us_char(KeyCode::NumpadAdd), Some('+'));
        assert_eq!(us_char(KeyCode::F5), None);
        assert_eq!(us_char(KeyCode::ArrowUp), None);
    }

    #[test]
    fn a_shortcut_follows_the_key_not_the_layout() {
        // The W key with an Azerbaijani layout types "ü".
        let c = pick("ü", CTRL, Some('w'), true).unwrap();
        assert_eq!(c, Chord::parse("Ctrl+W").unwrap());
        // A Russian layout: the C key types "с".
        let c = pick("с", (true, false, true, false), Some('c'), true).unwrap();
        assert_eq!(c, Chord::parse("Ctrl+Shift+C").unwrap());
        // Turned off: the typed character counts.
        let c = pick("ü", CTRL, Some('w'), false).unwrap();
        assert_eq!(c, Chord::parse("Ctrl+Ü").unwrap());
    }

    #[test]
    fn typing_and_named_keys_are_left_as_they_come() {
        // Without Ctrl, Alt or Super the key types: its text counts.
        let c = pick("ü", (false, false, true, false), Some('w'), true).unwrap();
        assert_eq!(
            c,
            Chord::from_event("ü", false, false, true, false).unwrap()
        );
        // Ctrl+F5: a named key, whatever key was noted.
        let c = pick("\u{F708}", CTRL, Some('w'), true).unwrap();
        assert_eq!(c, Chord::parse("Ctrl+F5").unwrap());
        // No key noted (another window had it): the text counts.
        let c = pick("x", CTRL, None, true).unwrap();
        assert_eq!(c, Chord::parse("Ctrl+X").unwrap());
        assert_eq!(pick("", CTRL, Some('a'), true), None);
    }
}
