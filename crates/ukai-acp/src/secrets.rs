//! Where `${env:NAME}` and `${keychain:NAME}` references read from outside
//! tests.

use ukai_config::Source;

/// The keychain service every `${keychain:NAME}` item is stored under; `NAME`
/// is the account.
pub const KEYCHAIN_SERVICE: &str = "ukai";

/// Read a reference from the process environment or the OS keychain.
///
/// Values are never logged. The keychain is only read on macOS; elsewhere a
/// `${keychain:...}` reference is reported as unreadable.
pub fn system_lookup(source: Source, name: &str) -> Option<String> {
    match source {
        Source::Env => std::env::var(name).ok(),
        Source::Keychain => keychain(name),
    }
}

#[cfg(target_os = "macos")]
fn keychain(name: &str) -> Option<String> {
    /// `errSecItemNotFound`: no such item, which needs no warning.
    const NOT_FOUND: i32 = -25300;
    match security_framework::passwords::get_generic_password(KEYCHAIN_SERVICE, name) {
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
