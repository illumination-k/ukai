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
