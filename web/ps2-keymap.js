// Map browser KeyboardEvent.code → PS/2 Set-1 scan codes, for feeding the
// guest's 8042 keyboard (Vm::push_scancode) so a graphical guest (Xorg via
// evdev/atkbd) sees real key events — distinct from the UART console path.
//
// A "make" (press) code is one byte (e.g. 0x1E for 'A'); the matching "break"
// (release) is the make OR 0x80. "Extended" keys (arrows, right-ctrl, …) are
// prefixed with 0xE0, and their break is 0xE0 then (code | 0x80). We push raw
// bytes in order, so multi-byte codes are just multiple push_scancode calls.

// code → make byte sequence (Set 1). Single-byte unless noted; 0xE0-prefixed
// entries are the extended keys.
const MAKE = {
  Escape: [0x01],
  Digit1: [0x02], Digit2: [0x03], Digit3: [0x04], Digit4: [0x05], Digit5: [0x06],
  Digit6: [0x07], Digit7: [0x08], Digit8: [0x09], Digit9: [0x0a], Digit0: [0x0b],
  Minus: [0x0c], Equal: [0x0d], Backspace: [0x0e], Tab: [0x0f],
  KeyQ: [0x10], KeyW: [0x11], KeyE: [0x12], KeyR: [0x13], KeyT: [0x14],
  KeyY: [0x15], KeyU: [0x16], KeyI: [0x17], KeyO: [0x18], KeyP: [0x19],
  BracketLeft: [0x1a], BracketRight: [0x1b], Enter: [0x1c], ControlLeft: [0x1d],
  KeyA: [0x1e], KeyS: [0x1f], KeyD: [0x20], KeyF: [0x21], KeyG: [0x22],
  KeyH: [0x23], KeyJ: [0x24], KeyK: [0x25], KeyL: [0x26],
  Semicolon: [0x27], Quote: [0x28], Backquote: [0x29], ShiftLeft: [0x2a], Backslash: [0x2b],
  KeyZ: [0x2c], KeyX: [0x2d], KeyC: [0x2e], KeyV: [0x2f], KeyB: [0x30],
  KeyN: [0x31], KeyM: [0x32], Comma: [0x33], Period: [0x34], Slash: [0x35],
  ShiftRight: [0x36], NumpadMultiply: [0x37], AltLeft: [0x38], Space: [0x39], CapsLock: [0x3a],
  F1: [0x3b], F2: [0x3c], F3: [0x3d], F4: [0x3e], F5: [0x3f],
  F6: [0x40], F7: [0x41], F8: [0x42], F9: [0x43], F10: [0x44],
  NumLock: [0x45], ScrollLock: [0x46], F11: [0x57], F12: [0x58],
  Numpad7: [0x47], Numpad8: [0x48], Numpad9: [0x49], NumpadSubtract: [0x4a],
  Numpad4: [0x4b], Numpad5: [0x4c], Numpad6: [0x4d], NumpadAdd: [0x4e],
  Numpad1: [0x4f], Numpad2: [0x50], Numpad3: [0x51], Numpad0: [0x52], NumpadDecimal: [0x53],
  // Extended (0xE0-prefixed).
  ControlRight: [0xe0, 0x1d], AltRight: [0xe0, 0x38],
  NumpadEnter: [0xe0, 0x1c], NumpadDivide: [0xe0, 0x35],
  Home: [0xe0, 0x47], ArrowUp: [0xe0, 0x48], PageUp: [0xe0, 0x49],
  ArrowLeft: [0xe0, 0x4b], ArrowRight: [0xe0, 0x4d],
  End: [0xe0, 0x4f], ArrowDown: [0xe0, 0x50], PageDown: [0xe0, 0x51],
  Insert: [0xe0, 0x52], Delete: [0xe0, 0x53],
  MetaLeft: [0xe0, 0x5b], MetaRight: [0xe0, 0x5c],
};

/** Make (key-press) scan-code bytes for a DOM `event.code`, or null. */
export function makeBytes(code) {
  return MAKE[code] || null;
}

/** Break (key-release) scan-code bytes for a DOM `event.code`, or null. */
export function breakBytes(code) {
  const m = MAKE[code];
  if (!m) return null;
  // Set the 0x80 release bit on the final code byte; keep any 0xE0 prefix.
  return m.length === 1 ? [m[0] | 0x80] : [m[0], m[1] | 0x80];
}

/**
 * Full scan-code sequence for a key combination given as an array of DOM
 * `event.code`s (modifiers first, e.g. ["ControlLeft","AltLeft","Delete"]).
 * Presses every key in order, then releases them in REVERSE order — exactly how
 * a real chord arrives — so the guest sees e.g. Ctrl↓ Alt↓ Del↓ Del↑ Alt↑ Ctrl↑.
 * Unknown codes are skipped. Returns a flat byte array for push_scancode.
 */
export function comboBytes(codes) {
  const out = [];
  for (const c of codes) {
    const m = makeBytes(c);
    if (m) out.push(...m);
  }
  for (let i = codes.length - 1; i >= 0; i--) {
    const b = breakBytes(codes[i]);
    if (b) out.push(...b);
  }
  return out;
}
