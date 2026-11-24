//! Where `${env:NAME}` and `${keychain:NAME}` references read from outside
//! tests.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use ukai_config::Source;

/// Read a reference from the process environment or the OS keychain.
///
/// Values are never logged. The keychain is only read on macOS; elsewhere a
/// `${keychain:...}` reference is reported as unreadable. An item read once
/// is kept for the life of the process: every pane, listing, and tunnel
/// resolves its endpoint again, and macOS may ask the user to allow each
/// read of an item another program stored. A renewed item is read after a
/// restart; one that could not be read is tried again next time.
pub fn system_lookup(source: Source, name: &str) -> Option<String> {
    static KEYCHAIN: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
    match source {
        Source::Env => std::env::var(name).ok(),
        Source::Keychain => remembered(KEYCHAIN.get_or_init(Default::default), name, keychain),
    }
}

/// `read(name)`, or what it returned for `name` before, when it returned
/// something.
fn remembered(
    cache: &Mutex<HashMap<String, String>>,
    name: &str,
    read: impl FnOnce(&str) -> Option<String>,
) -> Option<String> {
    let mut cache = cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(value) = cache.get(name) {
        return Some(value.clone());
    }
    let value = read(name)?;
    cache.insert(name.to_owned(), value.clone());
    Some(value)
}

#[cfg(target_os = "macos")]
fn keychain(name: &str) -> Option<String> {
    /// The service every `${keychain:NAME}` item is stored under; `NAME` is
    /// the account.
    const SERVICE: &str = "ukai";
    /// `errSecItemNotFound`: no such item, which needs no warning.
    const NOT_FOUND: i32 = -25300;
    match security_framework::passwords::get_generic_password(SERVICE, name) {
        Ok(bytes) => match String::from_utf8(bytes) {
            Ok(value) => Some(value),
            Err(_) => {
                tracing::warn!(item = name, "keychain item is not UTF-8");
                None
            }
        },
        Err(error) if error.code() == NOT_FOUND => None,
        Err(error) => {
            tracing::warn!(
                item = name,
                code = error.code(),
                "cannot read keychain item"
            );
            None
        }
    }
}

#[cfg(not(target_os = "macos"))]
fn keychain(name: &str) -> Option<String> {
    tracing::warn!(
        item = name,
        "`${{keychain:...}}` is only supported on macOS"
    );
    None
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;

    #[test]
    fn a_value_read_once_is_not_read_again() {
        let cache = Mutex::default();
        let reads = Cell::new(0);
        let read = |_: &str| {
            reads.set(reads.get() + 1);
            Some("token".to_owned())
        };
        assert_eq!(
            remembered(&cache, "gateway", read).as_deref(),
            Some("token")
        );
        assert_eq!(
            remembered(&cache, "gateway", read).as_deref(),
            Some("token")
        );
        assert_eq!(reads.get(), 1);
    }

    #[test]
    fn an_unreadable_item_is_tried_again() {
        let cache = Mutex::default();
        assert_eq!(remembered(&cache, "gateway", |_| None), None);
        assert_eq!(
            remembered(&cache, "gateway", |_| Some("token".to_owned())).as_deref(),
            Some("token")
        );
    }

    #[test]
    fn items_are_kept_by_name() {
        let cache = Mutex::default();
        remembered(&cache, "a", |_| Some("one".to_owned()));
        assert_eq!(
            remembered(&cache, "b", |_| Some("two".to_owned())).as_deref(),
            Some("two")
        );
    }
}
