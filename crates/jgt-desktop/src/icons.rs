//! Icon font and icon rendering helper for jg-team-dev.
//!
//! Uses a static, subsetted Material Symbols Outlined font bundled in `assets/`.
//! Glyphs are addressed by their Unicode Private Use Area code points.

#![allow(dead_code)] // used from step 4 on

use iced::{
    Font, Pixels,
    widget::{Text, text},
};

/// The bundled static subset of Material Symbols Outlined.
pub const FONT_BYTES: &[u8] = include_bytes!("../assets/material-symbols-outlined.ttf");

/// Font identifier referencing the bundled Material Symbols Outlined family.
pub const ICON_FONT: Font = Font::with_name("Material Symbols Outlined");

/// Icons used throughout the desktop interface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Icon {
    /// Board / Kanban view (`view_kanban`, U+EB7F)
    Board,
    /// Coordinator hub (`hub`, U+E9F4)
    Coordinator,
    /// Worker / robot (`smart_toy`, U+F06C)
    Worker,
    /// Archive folder/box (`inventory_2`, U+E1A1)
    Archive,
    /// Settings gear (`settings`, U+E8B8)
    Settings,
    /// Search magnifying glass (`search`, U+EF7A)
    Search,
    /// Listen toggler (`record_voice_over`, U+E91F)
    Listen,
    /// Replies toggler (`forum`, U+E8AF)
    Replies,
    /// Expand chevron downwards (`expand_more`, U+E5CF)
    ExpandMore,
    /// Collapse chevron rightwards (`chevron_right`, U+E5CC)
    ChevronRight,
    /// Add button (`add`, U+E145)
    Add,
    /// Close button (`close`, U+E5CD)
    Close,
    /// Workspace picker chevron (`unfold_more`, U+E5D7)
    UnfoldMore,
    /// Cost pill (`payments`, U+EF63)
    Cost,
    /// Working / in-progress sync spinner (`sync`, U+E627)
    Working,
    /// Checkmark / Done (`check`, U+E668)
    Check,
    /// Warning alert (`warning`, U+F083)
    Warning,
    /// Folder icon for workspace selector (`folder`, U+E2C7)
    Folder,
    /// Arrow upward (`arrow_upward`, U+E5D8)
    ArrowUpward,
    /// Arrow downward (`arrow_downward`, U+E5DB)
    ArrowDownward,
    /// Lock icon for secret files (`lock`, U+E897)
    Lock,
}

impl Icon {
    pub const ALL: &'static [Icon] = &[
        Icon::Board,
        Icon::Coordinator,
        Icon::Worker,
        Icon::Archive,
        Icon::Settings,
        Icon::Search,
        Icon::Listen,
        Icon::Replies,
        Icon::ExpandMore,
        Icon::ChevronRight,
        Icon::Add,
        Icon::Close,
        Icon::UnfoldMore,
        Icon::Cost,
        Icon::Working,
        Icon::Check,
        Icon::Warning,
        Icon::Folder,
        Icon::ArrowUpward,
        Icon::ArrowDownward,
        Icon::Lock,
    ];

    /// Unicode code point for this icon in the Private Use Area.
    #[inline]
    pub const fn codepoint(self) -> char {
        match self {
            Icon::Board => '\u{eb7f}',
            Icon::Coordinator => '\u{e9f4}',
            Icon::Worker => '\u{f06c}',
            Icon::Archive => '\u{e1a1}',
            Icon::Settings => '\u{e8b8}',
            Icon::Search => '\u{ef7a}',
            Icon::Listen => '\u{e91f}',
            Icon::Replies => '\u{e8af}',
            Icon::ExpandMore => '\u{e5cf}',
            Icon::ChevronRight => '\u{e5cc}',
            Icon::Add => '\u{e145}',
            Icon::Close => '\u{e5cd}',
            Icon::UnfoldMore => '\u{e5d7}',
            Icon::Cost => '\u{ef63}',
            Icon::Working => '\u{e627}',
            Icon::Check => '\u{e668}',
            Icon::Warning => '\u{f083}',
            Icon::Folder => '\u{e2c7}',
            Icon::ArrowUpward => '\u{e5d8}',
            Icon::ArrowDownward => '\u{e5db}',
            Icon::Lock => '\u{e897}',
        }
    }

    /// Alias for [`codepoint`](Self::codepoint).
    #[inline]
    pub const fn as_char(self) -> char {
        self.codepoint()
    }

    /// Single-character static string slice for this icon.
    #[inline]
    pub const fn as_str(self) -> &'static str {
        match self {
            Icon::Board => "\u{eb7f}",
            Icon::Coordinator => "\u{e9f4}",
            Icon::Worker => "\u{f06c}",
            Icon::Archive => "\u{e1a1}",
            Icon::Settings => "\u{e8b8}",
            Icon::Search => "\u{ef7a}",
            Icon::Listen => "\u{e91f}",
            Icon::Replies => "\u{e8af}",
            Icon::ExpandMore => "\u{e5cf}",
            Icon::ChevronRight => "\u{e5cc}",
            Icon::Add => "\u{e145}",
            Icon::Close => "\u{e5cd}",
            Icon::UnfoldMore => "\u{e5d7}",
            Icon::Cost => "\u{ef63}",
            Icon::Working => "\u{e627}",
            Icon::Check => "\u{e668}",
            Icon::Warning => "\u{f083}",
            Icon::Folder => "\u{e2c7}",
            Icon::ArrowUpward => "\u{e5d8}",
            Icon::ArrowDownward => "\u{e5db}",
            Icon::Lock => "\u{e897}",
        }
    }
}

/// Creates an iced `text` widget displaying the given icon at the specified size.
///
/// Colour and styling are left to the caller.
pub fn icon<'a>(icon: Icon, size: impl Into<Pixels>) -> Text<'a> {
    text(icon.as_str()).font(ICON_FONT).size(size)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn every_icon_maps_to_distinct_pua_codepoint() {
        let mut seen = HashSet::new();
        for &icon in Icon::ALL {
            let cp = icon.codepoint();
            let cp_u32 = cp as u32;

            // Must be non-ASCII
            assert!(!cp.is_ascii(), "icon {icon:?} must be non-ASCII");

            // Must be within Unicode BMP Private Use Area (U+E000 ..= U+F8FF)
            assert!(
                (0xe000..=0xf8ff).contains(&cp_u32),
                "icon {icon:?} ({cp_u32:#x}) must be in Unicode PUA range"
            );

            // Must be distinct
            assert!(
                seen.insert(cp),
                "duplicate codepoint {cp_u32:#x} for icon {icon:?}"
            );

            // as_str matches codepoint
            let s = icon.as_str();
            assert_eq!(s.chars().count(), 1);
            assert_eq!(s.chars().next(), Some(cp));
        }
    }

    #[test]
    fn font_bytes_are_embedded_and_parse() {
        assert!(!FONT_BYTES.is_empty(), "bundled font bytes must not be empty");
        assert!(FONT_BYTES.len() >= 4);
        let tag = &FONT_BYTES[0..4];
        let is_sfnt = tag == [0x00, 0x01, 0x00, 0x00] || tag == b"true" || tag == b"OTTO";
        assert!(is_sfnt, "font bytes must have a valid SFNT/TTF header");
    }

    #[test]
    fn icon_widget_construction() {
        let _ = icon(Icon::Settings, 16.0);
        let _ = icon(Icon::Board, 20);
    }
}
