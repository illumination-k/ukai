//! Browser panes: a web page served on a port of this machine, or of a
//! gateway agent's workspace through a tunnel (`ukai_acp::ports`).
//!
//! What the app needs to decide headless lives here: reading the port the
//! user typed, and telling which previews an agent asked for since the app
//! last looked.

use ukai_acp::ports::{Preview, preview_path};

/// The port and path the user means by `input`: `5173`, `:5173/docs`,
/// `localhost:5173`, or `http://127.0.0.1:5173/docs?q=1`. Only this
/// machine's own names are taken, since a pane opens a local port; `None`
/// for anything else.
pub fn parse_target(input: &str) -> Option<(u16, String)> {
    let input = input.trim();
    let rest = input
        .strip_prefix("http://")
        .or_else(|| input.strip_prefix("https://"))
        .unwrap_or(input);
    let rest = ["localhost", "127.0.0.1", "[::1]"]
        .iter()
        .find_map(|host| rest.strip_prefix(host))
        .unwrap_or(rest);
    let rest = rest.strip_prefix(':').unwrap_or(rest);
    let digits = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
    let (port, path) = rest.split_at(digits);
    let port: u16 = port.parse().ok().filter(|port| *port > 0)?;
    Some((port, preview_path(path)?))
}

/// The previews in `listing` newer than `seen`, the time of the newest one
/// the app already knew of, with the newest time now known. With no `seen`
/// — a workspace listed for the first time — none count as new: they were
/// asked for before the app was looking, and opening them all at once on a
/// restart would bury the layout.
pub fn new_previews(seen: Option<u64>, listing: &[Preview]) -> (Vec<Preview>, u64) {
    let newest = listing.iter().map(|preview| preview.at).max();
    let Some(seen) = seen else {
        return (Vec::new(), newest.unwrap_or(0));
    };
    let fresh: Vec<Preview> = listing
        .iter()
        .filter(|preview| preview.at > seen)
        .cloned()
        .collect();
    (fresh, newest.map_or(seen, |newest| newest.max(seen)))
}

/// How a pane names a page on `port` at `path`: `host:port`, then the path
/// unless it is the root.
pub fn label(host: &str, port: u16, path: &str) -> String {
    if path == "/" {
        format!("{host}:{port}")
    } else {
        format!("{host}:{port}{path}")
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    #[rstest]
    #[case::port("5173", Some((5173, "/")))]
    #[case::colon(":5173", Some((5173, "/")))]
    #[case::with_path("5173/docs", Some((5173, "/docs")))]
    #[case::localhost("localhost:3000", Some((3000, "/")))]
    #[case::loopback("127.0.0.1:3000/a?b=1", Some((3000, "/a?b=1")))]
    #[case::ipv6("[::1]:3000", Some((3000, "/")))]
    #[case::url("http://localhost:8080/admin/", Some((8080, "/admin/")))]
    #[case::https("https://localhost:8443", Some((8443, "/")))]
    #[case::spaces("  5173  ", Some((5173, "/")))]
    #[case::empty("", None)]
    #[case::zero("0", None)]
    #[case::too_large("70000", None)]
    #[case::other_host("example.com:80", None)]
    #[case::no_port("localhost", None)]
    #[case::trailing_garbage("5173docs", None)]
    fn reads_the_port_the_user_typed(#[case] input: &str, #[case] expected: Option<(u16, &str)>) {
        assert_eq!(
            parse_target(input),
            expected.map(|(port, path)| (port, path.to_owned()))
        );
    }

    fn preview(port: u16, at: u64) -> Preview {
        Preview {
            port,
            path: "/".into(),
            title: None,
            at,
        }
    }

    #[test]
    fn opens_nothing_the_app_was_not_looking_for() {
        let (fresh, seen) = new_previews(None, &[preview(3000, 5), preview(3001, 9)]);
        assert!(fresh.is_empty());
        assert_eq!(seen, 9);
        assert_eq!(new_previews(None, &[]), (Vec::new(), 0));
    }

    #[test]
    fn opens_what_was_asked_for_since() {
        let listing = [preview(3000, 5), preview(3001, 9), preview(3002, 12)];
        let (fresh, seen) = new_previews(Some(5), &listing);
        assert_eq!(fresh, [preview(3001, 9), preview(3002, 12)]);
        assert_eq!(seen, 12);
        let (fresh, seen) = new_previews(Some(12), &listing);
        assert!(fresh.is_empty());
        assert_eq!(seen, 12);
        // A relay that started over lists less; what was seen stays seen.
        assert_eq!(new_previews(Some(12), &[]), (Vec::new(), 12));
    }

    #[rstest]
    #[case::root("localhost", 5173, "/", "localhost:5173")]
    #[case::path("claude", 3000, "/docs", "claude:3000/docs")]
    fn names_a_page(
        #[case] host: &str,
        #[case] port: u16,
        #[case] path: &str,
        #[case] expected: &str,
    ) {
        assert_eq!(label(host, port, path), expected);
    }
}
