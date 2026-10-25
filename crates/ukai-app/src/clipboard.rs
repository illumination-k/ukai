//! The system clipboard, apart from the web view: writing text from a
//! button, and reading the files and images a paste should attach.

use std::cell::RefCell;
use std::path::PathBuf;

use image::ImageEncoder;

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

/// What a paste brings that the web view does not insert itself.
pub enum Pasted {
    /// Files copied in a file manager.
    Files(Vec<PathBuf>),
    /// An image with no text alongside it, such as a screenshot, as PNG.
    Image(Vec<u8>),
}

/// What is on the clipboard for a paste to attach, if anything. Text is the
/// web view's to paste, so an image that comes with text is left alone.
pub fn pasted() -> Option<Pasted> {
    CLIPBOARD.with_borrow_mut(|slot| {
        let clipboard = match slot {
            Some(clipboard) => clipboard,
            None => slot.insert(
                arboard::Clipboard::new()
                    .inspect_err(|error| tracing::warn!("no clipboard: {error}"))
                    .ok()?,
            ),
        };
        if let Ok(files) = clipboard.get().file_list()
            && !files.is_empty()
        {
            return Some(Pasted::Files(files));
        }
        if clipboard.get_text().is_ok_and(|text| !text.is_empty()) {
            return None;
        }
        let image = clipboard.get_image().ok()?;
        png(&image)
            .inspect_err(|error| tracing::warn!("pasted image: {error}"))
            .ok()
            .map(Pasted::Image)
    })
}

fn png(image: &arboard::ImageData) -> image::ImageResult<Vec<u8>> {
    let mut bytes = Vec::new();
    image::codecs::png::PngEncoder::new(&mut bytes).write_image(
        &image.bytes,
        u32::try_from(image.width).unwrap_or(u32::MAX),
        u32::try_from(image.height).unwrap_or(u32::MAX),
        image::ExtendedColorType::Rgba8,
    )?;
    Ok(bytes)
}
