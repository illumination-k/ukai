//! Stroke icons drawn inline, so they follow `currentColor` and need no font.

use dioxus::prelude::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Icon {
    Plus,
    Close,
    SplitRight,
    SplitDown,
    Reload,
    Folder,
    Send,
    Stop,
    Chevron,
    Document,
    External,
    Paperclip,
    Search,
    Copy,
    Check,
    Mic,
}

impl Icon {
    /// Path data on a 16×16 grid.
    fn paths(self) -> &'static [&'static str] {
        match self {
            Self::Plus => &["M8 3.5v9", "M3.5 8h9"],
            Self::Close => &["M4.5 4.5l7 7", "M11.5 4.5l-7 7"],
            Self::SplitRight => &["M2.5 3.5h11v9h-11z", "M8 3.5v9"],
            Self::SplitDown => &["M2.5 3.5h11v9h-11z", "M2.5 8h11"],
            Self::Reload => &["M12.5 8a4.5 4.5 0 1 1-1.3-3.2", "M12.5 2.5v2.8h-2.8"],
            Self::Folder => &[
                "M2 4.5c0-.6.4-1 1-1h3l1.5 1.5H13c.6 0 1 .4 1 1v6c0 .6-.4 1-1 1H3c-.6 0-1-.4-1-1z",
            ],
            Self::Send => &["M8 12.5v-9", "M4 7.5l4-4 4 4"],
            Self::Stop => &["M5.5 5.5h5v5h-5z"],
            Self::Chevron => &["M6 4l4 4-4 4"],
            Self::Document => &["M4 2.5h5l3 3v8H4z", "M9 2.5v3h3", "M6 9h4", "M6 11h4"],
            Self::External => &["M9 3h4v4", "M13 3l-6 6", "M11 9.5v3H3.5V5h3"],
            Self::Paperclip => &[
                "M12.5 7.5l-4.8 4.8a2.8 2.8 0 0 1-4-4l5-5a1.9 1.9 0 0 1 2.7 2.7l-5 5a.9.9 0 0 1-1.3-1.3l4.6-4.6",
            ],
            Self::Search => &[
                "M7 2.5a4.5 4.5 0 1 0 0 9 4.5 4.5 0 1 0 0-9z",
                "M10.3 10.3l3.2 3.2",
            ],
            Self::Copy => &["M5.5 5.5h8v8h-8z", "M10.5 5.5v-3h-8v8h3"],
            Self::Check => &["M3.5 8.5l3 3 6-7"],
            Self::Mic => &[
                "M6 4.5a2 2 0 0 1 4 0v3a2 2 0 0 1-4 0z",
                "M3.5 7.5a4.5 4.5 0 0 0 9 0",
                "M8 12v2",
            ],
        }
    }
}

#[component]
pub fn Svg(icon: Icon, #[props(default = 14)] size: u32) -> Element {
    rsx! {
        svg {
            class: "glyph",
            width: "{size}",
            height: "{size}",
            view_box: "0 0 16 16",
            fill: "none",
            stroke: "currentColor",
            stroke_width: "1.4",
            stroke_linecap: "round",
            stroke_linejoin: "round",
            "aria-hidden": "true",
            for d in icon.paths() {
                path { d: *d }
            }
        }
    }
}
