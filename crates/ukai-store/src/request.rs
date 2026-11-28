//! What every service here asks of a request before its store is touched.

use tokio::sync::OnceCell;
use tonic::Status;

/// The store, once it is reached; until then every call is UNAVAILABLE.
/// `what` is what it keeps, for the message.
pub fn reached<'a, T>(store: &'a OnceCell<T>, what: &str) -> Result<&'a T, Status> {
    store
        .get()
        .ok_or_else(|| Status::unavailable(format!("the {what} store is not reached yet")))
}

/// The workspace a request names, or INVALID_ARGUMENT when it names none.
pub fn workspace(workspace: &str) -> Result<&str, Status> {
    if workspace.is_empty() {
        Err(Status::invalid_argument("the request names no workspace"))
    } else {
        Ok(workspace)
    }
}

#[cfg(test)]
mod tests {
    use tonic::Code;

    use super::*;

    #[test]
    fn a_store_not_reached_yet_is_unavailable() {
        let store = OnceCell::new();
        assert_eq!(
            reached(&store, "reports").map_err(|status| status.code()),
            Err(Code::Unavailable)
        );
        assert!(store.set(1).is_ok());
        assert_eq!(reached(&store, "reports").ok(), Some(&1));
    }

    #[test]
    fn a_request_names_its_workspace() {
        assert_eq!(workspace("w").ok(), Some("w"));
        assert_eq!(
            workspace("").map_err(|status| status.code()),
            Err(Code::InvalidArgument)
        );
    }
}
