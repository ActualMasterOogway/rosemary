//! Pointer and keyboard input for a page, delivered as DOM events.
//!
//! GTK 4 has no public way to make an input event: a widget receives them only
//! from its surface, and a view that is never on screen has none. So AppKit's
//! events are replayed into the page from script, run in a world of its own so
//! that the page can neither see the helper nor replace it. Hit testing, focus,
//! text insertion through the editing machinery (`insertText`, which fires the
//! `beforeinput` and `input` events frameworks listen for), form submission and
//! scrolling all behave as they would for a person.
//!
//! The helper is installed as a user script in its world rather than sent with
//! each event. WebKitGTK keeps a named world, and the globals scripts leave in
//! it, only while a user script is registered for it: without one, every
//! evaluation gets a fresh world, and a release would no longer know which
//! element was pressed, so no click would ever be dispatched.
//!
//! FIXME (ACCURACY): the events are untrusted (`isTrusted` is false), `:hover`
//! does not follow the pointer, a click does not place the caret inside a text
//! field, and there is no input method. A page that checks for trusted events -
//! some captchas do - will not accept them.

/// The script world the helper runs in.
pub const WORLD: &std::ffi::CStr = c"rosetta3-input";

/// Defines `window.__rosetta3` in the world once per document.
pub const HELPER: &str = r#"
if (!window.__rosetta3) window.__rosetta3 = (() => {
  const focusable = 'input,textarea,select,button,a[href],[tabindex],[contenteditable=""],[contenteditable="true"],summary';
  let pressed = null, hovered = null;
  const at = (x, y) => document.elementFromPoint(x, y) || document.body || document.documentElement;
  const modifiers = (m) => ({ shiftKey: !!(m & 1), ctrlKey: !!(m & 2), altKey: !!(m & 4), metaKey: !!(m & 8) });
  function mouse(type, target, x, y, button, buttons, m, detail) {
    if (!target) return true;
    const init = Object.assign({ bubbles: true, cancelable: true, composed: true, view: window,
      clientX: x, clientY: y, screenX: x, screenY: y, button, buttons, detail }, modifiers(m));
    const pointer = { mousedown: 'pointerdown', mouseup: 'pointerup', mousemove: 'pointermove' }[type];
    if (pointer && typeof PointerEvent === 'function') {
      target.dispatchEvent(new PointerEvent(pointer, Object.assign({ pointerId: 1, pointerType: 'mouse', isPrimary: true }, init)));
    }
    return target.dispatchEvent(new MouseEvent(type, init));
  }
  function hover(target, x, y, buttons, m) {
    if (target === hovered) return;
    const old = hovered;
    hovered = target;
    if (old && old.isConnected) mouse('mouseout', old, x, y, 0, buttons, m, 0);
    if (target) mouse('mouseover', target, x, y, 0, buttons, m, 0);
  }
  const editable = (e) => e && (e.isContentEditable || e.tagName === 'TEXTAREA' ||
    (e.tagName === 'INPUT' && !/^(button|submit|reset|checkbox|radio|file|image|range|color|hidden)$/i.test(e.type)));
  function tabbable() {
    return Array.from(document.querySelectorAll(focusable)).filter((e) =>
      !e.disabled && e.tabIndex >= 0 && e.getClientRects().length > 0);
  }
  function scroller(from, dx, dy) {
    for (let e = from; e && e !== document.documentElement; e = e.parentElement) {
      const style = getComputedStyle(e);
      if (dy && /(auto|scroll|overlay)/.test(style.overflowY) && e.scrollHeight > e.clientHeight) return e;
      if (dx && /(auto|scroll|overlay)/.test(style.overflowX) && e.scrollWidth > e.clientWidth) return e;
    }
    return document.scrollingElement || document.documentElement;
  }
  return {
    move(x, y, buttons, m) {
      const target = at(x, y);
      hover(target, x, y, buttons, m);
      mouse('mousemove', target, x, y, 0, buttons, m, 0);
    },
    down(x, y, button, count, m) {
      const target = at(x, y);
      hover(target, x, y, 1 << button, m);
      pressed = target;
      const allowed = mouse('mousedown', target, x, y, button, button === 2 ? 2 : 1 << button, m, count);
      if (allowed && button === 0) {
        const focus = target && target.closest ? target.closest(focusable) : null;
        if (focus && !focus.disabled) {
          if (document.activeElement !== focus) focus.focus();
        } else if (document.activeElement && document.activeElement !== document.body) {
          document.activeElement.blur();
        }
      }
    },
    up(x, y, button, count, m) {
      const target = at(x, y);
      mouse('mouseup', target, x, y, button, 0, m, count);
      const start = pressed;
      pressed = null;
      if (!start || !target) return;
      let common = start;
      while (common && !common.contains(target)) common = common.parentElement;
      if (!common) return;
      if (button === 0) {
        mouse('click', common, x, y, 0, 0, m, count);
        if (count === 2) mouse('dblclick', common, x, y, 0, 0, m, count);
      } else if (button === 2) {
        mouse('contextmenu', common, x, y, 2, 0, m, count);
      }
    },
    wheel(x, y, dx, dy, m) {
      const target = at(x, y);
      const init = Object.assign({ bubbles: true, cancelable: true, composed: true, view: window,
        clientX: x, clientY: y, deltaX: dx, deltaY: dy, deltaMode: 0 }, modifiers(m));
      if (!target.dispatchEvent(new WheelEvent('wheel', init))) return;
      scroller(target, dx, dy).scrollBy({ left: dx, top: dy, behavior: 'instant' });
    },
    key(type, key, code, keyCode, m, repeat) {
      const target = document.activeElement || document.body;
      const init = Object.assign({ bubbles: true, cancelable: true, composed: true, view: window,
        key, code, keyCode, which: keyCode, repeat }, modifiers(m));
      return target.dispatchEvent(new KeyboardEvent(type, init));
    },
    text(text) {
      const target = document.activeElement;
      if (!editable(target)) return;
      if (!document.execCommand('insertText', false, text) && 'setRangeText' in target) {
        target.setRangeText(text, target.selectionStart, target.selectionEnd, 'end');
        target.dispatchEvent(new InputEvent('input', { bubbles: true, inputType: 'insertText', data: text }));
      }
    },
    edit(key, m) {
      const target = document.activeElement;
      const shift = !!(m & 1), word = !!(m & 4), line = !!(m & 8);
      const extend = shift ? 'extend' : 'move';
      const selection = window.getSelection();
      switch (key) {
        case 'Backspace':
          if (editable(target)) document.execCommand(line || word ? 'deleteWordBackward' : 'delete');
          return;
        case 'Delete':
          if (editable(target)) document.execCommand('forwardDelete');
          return;
        case 'Tab': {
          const all = tabbable();
          if (all.length === 0) return;
          const index = all.indexOf(target);
          const next = shift ? (index <= 0 ? all.length - 1 : index - 1) : (index + 1) % all.length;
          all[next].focus();
          return;
        }
        case 'Enter':
          if (target && (target.isContentEditable || target.tagName === 'TEXTAREA')) {
            document.execCommand('insertLineBreak');
          } else if (target && target.form) {
            const form = target.form;
            const submitter = form.querySelector('button:not([type]),button[type=submit],input[type=submit],input[type=image]');
            if (submitter && !submitter.disabled) submitter.click();
            else if (form.requestSubmit) form.requestSubmit();
          } else if (target && target !== document.body) {
            target.click();
          }
          return;
        case ' ':
          if (target && !editable(target) && /^(BUTTON|SUMMARY)$|^INPUT$/.test(target.tagName)) target.click();
          return;
        case 'ArrowLeft': case 'ArrowRight': {
          if (!editable(target)) break;
          const forward = key === 'ArrowRight';
          selection.modify(extend, forward ? 'forward' : 'backward', line ? 'lineboundary' : word ? 'word' : 'character');
          return;
        }
        case 'ArrowUp': case 'ArrowDown':
          if (editable(target) && target.tagName !== 'INPUT') {
            selection.modify(extend, key === 'ArrowDown' ? 'forward' : 'backward', 'line');
            return;
          }
          break;
        case 'Home': case 'End':
          if (editable(target)) {
            selection.modify(extend, key === 'End' ? 'forward' : 'backward', 'lineboundary');
            return;
          }
          break;
      }
      const page = document.scrollingElement || document.documentElement;
      const step = { ArrowUp: -40, ArrowDown: 40, PageUp: -innerHeight * 0.875, PageDown: innerHeight * 0.875,
        Home: -page.scrollHeight, End: page.scrollHeight, ' ': shift ? -innerHeight * 0.875 : innerHeight * 0.875 }[key];
      if (step && !editable(target)) page.scrollBy({ top: step, behavior: 'instant' });
    },
  };
})();
"#;

/// Modifier bits the helper reads: shift, control, option, command.
pub fn modifier_bits(flags: u64) -> u32 {
    const SHIFT: u64 = 1 << 17;
    const CONTROL: u64 = 1 << 18;
    const OPTION: u64 = 1 << 19;
    const COMMAND: u64 = 1 << 20;
    u32::from(flags & SHIFT != 0)
        | u32::from(flags & CONTROL != 0) << 1
        | u32::from(flags & OPTION != 0) << 2
        | u32::from(flags & COMMAND != 0) << 3
}

/// A JSON string literal, which is also a valid JavaScript one.
pub fn quote(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            // Line and paragraph separators end a line in older engines' string
            // literals; control characters are not allowed in one at all.
            '\u{2028}' | '\u{2029}' => out.push_str(&format!("\\u{:04x}", c as u32)),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// A script running `body` with the helper as `r`, if the document has it.
pub fn call(body: &str) -> String {
    format!("((r) => {{ if (r) {{ {body}; }} }})(window.__rosetta3);")
}

/// A key as the DOM names it: `key`, `code` and the legacy `keyCode`.
pub struct Key {
    pub key: String,
    pub code: &'static str,
    pub key_code: u32,
    /// Whether the key edits or navigates rather than inserting its text.
    pub special: bool,
}

/// The DOM's names for a Mac virtual key code and the characters it produced.
pub fn key(mac_key_code: u16, characters: &str) -> Key {
    let named = |key: &str, code: &'static str, key_code: u32| Key {
        key: key.to_string(),
        code,
        key_code,
        special: true,
    };
    match mac_key_code {
        0x24 | 0x4C => named("Enter", "Enter", 13),
        0x30 => named("Tab", "Tab", 9),
        0x31 => named(" ", "Space", 32),
        0x33 => named("Backspace", "Backspace", 8),
        0x35 => named("Escape", "Escape", 27),
        0x75 => named("Delete", "Delete", 46),
        0x73 => named("Home", "Home", 36),
        0x77 => named("End", "End", 35),
        0x74 => named("PageUp", "PageUp", 33),
        0x79 => named("PageDown", "PageDown", 34),
        0x7B => named("ArrowLeft", "ArrowLeft", 37),
        0x7C => named("ArrowRight", "ArrowRight", 39),
        0x7D => named("ArrowDown", "ArrowDown", 40),
        0x7E => named("ArrowUp", "ArrowUp", 38),
        _ => {
            let key = characters
                .chars()
                .filter(|c| !c.is_control() && !('\u{F700}'..='\u{F8FF}').contains(c))
                .collect::<String>();
            let key_code = key
                .chars()
                .next()
                .map_or(0, |c| c.to_ascii_uppercase() as u32);
            Key {
                key: if key.is_empty() {
                    "Unidentified".to_string()
                } else {
                    key
                },
                code: "",
                key_code,
                special: false,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting_escapes_what_ends_a_string_literal() {
        assert_eq!(
            quote("a\"b\\c\nd\u{2028}\u{1}"),
            "\"a\\\"b\\\\c\\nd\\u2028\\u0001\""
        );
    }

    #[test]
    fn named_keys_edit_and_printable_keys_insert() {
        let backspace = key(0x33, "\u{7f}");
        assert_eq!(
            (backspace.key.as_str(), backspace.special),
            ("Backspace", true)
        );
        let letter = key(0x00, "a");
        assert_eq!(
            (letter.key.as_str(), letter.key_code, letter.special),
            ("a", 65, false)
        );
        // An arrow's private-use character is not text.
        assert_eq!(key(0x60, "\u{F704}").key, "Unidentified");
    }

    #[test]
    fn modifier_flags_map_to_the_helper_bits() {
        assert_eq!(modifier_bits((1 << 17) | (1 << 20)), 0b1001);
    }
}
