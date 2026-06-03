//! Host-side ASCII → PS/2 Set-1 scan-code translation.
//!
//! Turns a string the host wants to "type" into the raw scan-code byte
//! stream a US-QWERTY keyboard would emit, for feeding a graphical guest's
//! 8042 keyboard via [`crate::Keyboard::push_scancode`] (so Xorg/evdev see
//! real key events). This is the native counterpart of the browser's
//! `web/ps2-keymap.js` (which maps DOM `event.code`); here we map characters.
//!
//! A key press is the "make" code; release is `make | 0x80` ("break").
//! Shifted characters (uppercase, `!@#…`) are wrapped in Left-Shift
//! make/break (0x2A / 0xAA). Characters with no US-QWERTY key are skipped.

/// Left-Shift make / break (Set 1).
const SHIFT_MAKE: u8 = 0x2A;
const SHIFT_BREAK: u8 = 0xAA;

/// The Set-1 make code for a character, plus whether Shift is held. Returns
/// `None` for characters that aren't on a US-QWERTY keyboard.
fn key_for(c: char) -> Option<(u8, bool)> {
    // Unshifted keys.
    let unshifted = |c: char| -> Option<u8> {
        Some(match c {
            '1' => 0x02,
            '2' => 0x03,
            '3' => 0x04,
            '4' => 0x05,
            '5' => 0x06,
            '6' => 0x07,
            '7' => 0x08,
            '8' => 0x09,
            '9' => 0x0A,
            '0' => 0x0B,
            '-' => 0x0C,
            '=' => 0x0D,
            '\t' => 0x0F,
            'q' => 0x10,
            'w' => 0x11,
            'e' => 0x12,
            'r' => 0x13,
            't' => 0x14,
            'y' => 0x15,
            'u' => 0x16,
            'i' => 0x17,
            'o' => 0x18,
            'p' => 0x19,
            '[' => 0x1A,
            ']' => 0x1B,
            '\n' => 0x1C,
            'a' => 0x1E,
            's' => 0x1F,
            'd' => 0x20,
            'f' => 0x21,
            'g' => 0x22,
            'h' => 0x23,
            'j' => 0x24,
            'k' => 0x25,
            'l' => 0x26,
            ';' => 0x27,
            '\'' => 0x28,
            '`' => 0x29,
            '\\' => 0x2B,
            'z' => 0x2C,
            'x' => 0x2D,
            'c' => 0x2E,
            'v' => 0x2F,
            'b' => 0x30,
            'n' => 0x31,
            'm' => 0x32,
            ',' => 0x33,
            '.' => 0x34,
            '/' => 0x35,
            ' ' => 0x39,
            _ => return None,
        })
    };
    if let Some(code) = unshifted(c) {
        return Some((code, false));
    }
    // Shifted keys: map to the base key's code, Shift held. Lowercase the
    // letters; translate the shifted punctuation to its base character.
    if c.is_ascii_uppercase() {
        return unshifted(c.to_ascii_lowercase()).map(|code| (code, true));
    }
    let base = match c {
        '!' => '1',
        '@' => '2',
        '#' => '3',
        '$' => '4',
        '%' => '5',
        '^' => '6',
        '&' => '7',
        '*' => '8',
        '(' => '9',
        ')' => '0',
        '_' => '-',
        '+' => '=',
        '{' => '[',
        '}' => ']',
        '|' => '\\',
        ':' => ';',
        '"' => '\'',
        '~' => '`',
        '<' => ',',
        '>' => '.',
        '?' => '/',
        _ => return None,
    };
    unshifted(base).map(|code| (code, true))
}

/// Append the scan-code bytes for one character to `out` (make + break,
/// wrapped in Shift if needed). No-op for unmapped characters.
pub fn push_char_scancodes(out: &mut Vec<u8>, c: char) {
    if let Some((make, shift)) = key_for(c) {
        if shift {
            out.push(SHIFT_MAKE);
        }
        out.push(make);
        out.push(make | 0x80);
        if shift {
            out.push(SHIFT_BREAK);
        }
    }
}

/// The full Set-1 scan-code byte stream for typing `s` on a US-QWERTY
/// keyboard. Unmapped characters are silently skipped.
pub fn string_to_scancodes(s: &str) -> Vec<u8> {
    let mut out = Vec::new();
    for c in s.chars() {
        push_char_scancodes(&mut out, c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lowercase_letter_is_make_then_break() {
        assert_eq!(string_to_scancodes("a"), vec![0x1E, 0x9E]);
    }

    #[test]
    fn uppercase_letter_wraps_in_shift() {
        // Shift make, A make, A break, Shift break.
        assert_eq!(string_to_scancodes("A"), vec![0x2A, 0x1E, 0x9E, 0xAA]);
    }

    #[test]
    fn digit_and_shifted_symbol() {
        assert_eq!(string_to_scancodes("1"), vec![0x02, 0x82]);
        // '!' is Shift + '1'.
        assert_eq!(string_to_scancodes("!"), vec![0x2A, 0x02, 0x82, 0xAA]);
    }

    #[test]
    fn whitespace_and_enter() {
        assert_eq!(string_to_scancodes(" "), vec![0x39, 0xB9]);
        assert_eq!(string_to_scancodes("\n"), vec![0x1C, 0x9C]);
        assert_eq!(string_to_scancodes("\t"), vec![0x0F, 0x8F]);
    }

    #[test]
    fn word_concatenates_in_order() {
        // "Hi" = Shift+h, i.
        assert_eq!(
            string_to_scancodes("Hi"),
            vec![0x2A, 0x23, 0xA3, 0xAA, 0x17, 0x97]
        );
    }

    #[test]
    fn unmapped_characters_are_skipped() {
        // A non-US-QWERTY char (e.g. 'é') produces nothing; surrounding
        // mapped chars still translate.
        assert_eq!(string_to_scancodes("é"), Vec::<u8>::new());
        assert_eq!(string_to_scancodes("aéb"), vec![0x1E, 0x9E, 0x30, 0xB0]);
    }

    #[test]
    fn shifted_punctuation_maps_to_base_key_with_shift() {
        // ':' is Shift + ';' (0x27).
        assert_eq!(string_to_scancodes(":"), vec![0x2A, 0x27, 0xA7, 0xAA]);
        // '?' is Shift + '/' (0x35).
        assert_eq!(string_to_scancodes("?"), vec![0x2A, 0x35, 0xB5, 0xAA]);
    }
}
