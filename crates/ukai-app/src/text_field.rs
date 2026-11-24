//! Text fields that keep what the user types (`ukai_core::field`). Never
//! bind an `input`'s or a `textarea`'s `value`: give it the id of a
//! [`use_text_field`], and tell the field what was typed and when its
//! element mounts.

use std::sync::atomic::{AtomicU64, Ordering};

use dioxus::prelude::*;
use ukai_core::field::Field;

#[derive(Clone, Copy, PartialEq)]
pub struct TextField {
    id: Signal<String>,
    field: Signal<Field>,
    mounts: Signal<u64>,
}

/// A text field showing what `value` reads, written into the element only
/// when it is not what the user typed.
pub fn use_text_field(mut value: impl FnMut() -> String + 'static) -> TextField {
    let id = use_signal(|| {
        static FIELDS: AtomicU64 = AtomicU64::new(0);
        format!("text-field-{}", FIELDS.fetch_add(1, Ordering::Relaxed))
    });
    let mut field = use_signal(Field::default);
    let mounts = use_signal(|| 0_u64);
    use_effect(move || {
        mounts.read();
        let value = value();
        if let Some(value) = field.write().wanted(&value) {
            write(&id.peek(), value);
        }
    });
    TextField { id, field, mounts }
}

impl TextField {
    /// For the element's `id`.
    pub fn id(&self) -> String {
        self.id.peek().clone()
    }

    /// For `oninput`, before the value is taken: the user typed `value`.
    pub fn typed(mut self, value: &str) {
        self.field.write().typed(value);
    }

    /// For `onmounted`: the new element is given the value.
    pub fn mounted(mut self) {
        self.field.write().mounted();
        self.mounts += 1;
    }
}

/// Put `value` in the element `id`, with the cursor at its end.
fn write(id: &str, value: String) {
    let eval = document::eval(&format!(
        "const value = await dioxus.recv();
         const input = document.getElementById('{id}');
         if (input && input.value !== value) {{
             input.value = value;
             input.selectionStart = input.selectionEnd = value.length;
         }}"
    ));
    if let Err(error) = eval.send(value) {
        tracing::warn!("text field: {error}");
    }
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::path::Path;

    /// Each keystroke bound back through `value` is a round trip late, so it
    /// drops keys and breaks input methods; see `ukai_core::field`.
    #[test]
    fn no_text_field_binds_its_value() -> io::Result<()> {
        let mut bound = Vec::new();
        visit(
            &Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
            &mut bound,
        )?;
        assert!(
            bound.is_empty(),
            "use `use_text_field` instead of binding `value`:\n{}",
            bound.join("\n")
        );
        Ok(())
    }

    fn visit(dir: &Path, bound: &mut Vec<String>) -> io::Result<()> {
        for entry in std::fs::read_dir(dir)? {
            let path = entry?.path();
            if path.is_dir() {
                visit(&path, bound)?;
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                let source = std::fs::read_to_string(&path)?;
                for (n, line) in source.lines().enumerate() {
                    if line.trim_start().starts_with("value: \"") {
                        bound.push(format!("{}:{}", path.display(), n + 1));
                    }
                }
            }
        }
        Ok(())
    }
}
