//! Writing text to the system clipboard from a button, independent of the
//! web view's own selection and copy handling.

use std::cell::RefCell;

thread_local! {
    /// Kept for the app's lifetime: on X11 the clipboard owner serves the
    /// text, and dropping it can take the text with it.
    static CLIPBOARD: RefCell<Option<arboard::Clipboard>> = const { RefCell::new(None) };
}

/// Put `text` on the clipboard. Failures are logged, not shown.
pub fn copy(text: &str) -> bool {
    CLIPBOARD.with_borrow_mut(|slot| {
        let clipboard = match slot {
            Some(clipboard) => clipboard,
            None => match arboard::Clipboard::new() {
                Ok(clipboard) => slot.insert(clipboard),
                Err(error) => {
                    tracing::warn!("no clipboard: {error}");
                    return false;
                }
            },
        };
        clipboard
            .set_text(text)
            .inspect_err(|error| tracing::warn!("copy failed: {error}"))
            .is_ok()
    })
}
