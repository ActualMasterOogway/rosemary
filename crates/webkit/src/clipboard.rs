//! The host's clipboard, as the selection of GTK's private display.
//!
//! GTK's clipboard lives in the web view compositor, which no other program
//! can see. What a page copies is moved to the general pasteboard, and the
//! pasteboard is offered back to GTK before a page can paste.

use std::cell::Cell;

use crate::compositor::Command;
use crate::engine::Engine;

/// The types GTK asks for text by, all answered with the same bytes.
const TEXT_TYPES: &[&str] = &[
    "text/plain;charset=utf-8",
    "UTF8_STRING",
    "text/plain",
    "TEXT",
    "STRING",
];
const HTML_TYPE: &str = "text/html";

thread_local! {
    /// The pasteboard's change count when GTK last saw its contents, either
    /// because they were offered or because they came from GTK.
    static SEEN: Cell<Option<isize>> = const { Cell::new(None) };
}

/// Offer the pasteboard to GTK, unless it already has what is on it.
///
/// Called when a web view takes the keyboard and just before a paste, rather
/// than on every turn, because reading the desktop's clipboard can be slow.
pub fn offer(engine: &Engine) {
    let (count, text, html) = appkit::pasteboard::general_text_and_html();
    if SEEN.get() == Some(count) {
        return;
    }
    SEEN.set(Some(count));
    let mut entries = Vec::new();
    if let Some(text) = text {
        entries.extend(TEXT_TYPES.iter().map(|ty| (ty.to_string(), text.clone())));
    }
    if let Some(html) = html {
        entries.push((HTML_TYPE.to_string(), html));
    }
    if !entries.is_empty() {
        engine.compositor.send(Command::Offer { entries });
    }
}

/// A page copied: put what it copied on the pasteboard.
pub fn copied(entries: Vec<(String, Vec<u8>)>) {
    if crate::engine::tracing() {
        let types: Vec<&str> = entries.iter().map(|(ty, _)| ty.as_str()).collect();
        eprintln!("rosemary: webkit: the page copied {types:?}");
    }
    let mut text = None;
    let mut html = None;
    for (ty, bytes) in entries {
        match ty.as_str() {
            HTML_TYPE => html = Some(bytes),
            _ if TEXT_TYPES.contains(&ty.as_str()) => text = Some(bytes),
            _ => {}
        }
    }
    if text.is_none() && html.is_none() {
        return;
    }
    let count = appkit::pasteboard::set_general_text_and_html(text, html);
    SEEN.set(Some(count));
}
