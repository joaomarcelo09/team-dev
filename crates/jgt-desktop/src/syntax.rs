//! Syntax highlighting for the IDE file viewer.
//!
//! A small `iced` highlighter on top of syntect (the same engine and syntax set that
//! `iced_highlighter` uses), with code colors that follow the app theme: Dracula and
//! Tokyo Night get their own palettes, built in code; the other themes keep iced's
//! Base16 Mocha (dark) / InspiredGitHub (light) looks.
//!
//! The line-by-line parsing with snapshots every 50 lines mirrors `iced_highlighter`,
//! so large files cost the same as before.

use crate::app::ThemeStyle;
use iced::advanced::text::highlighter::{self, Format};
use iced::{Color, Font, font};
use std::ops::Range;
use std::str::FromStr;
use std::sync::LazyLock;
use two_face::re_exports::syntect::highlighting::{
    self as hl, FontStyle, ScopeSelectors, StyleModifier, ThemeItem, ThemeSettings,
};
use two_face::re_exports::syntect::parsing::{
    ParseState, ScopeStack, ScopeStackOp, SyntaxReference, SyntaxSet,
};

static SYNTAXES: LazyLock<SyntaxSet> = LazyLock::new(two_face::syntax::extra_no_newlines);
static DEFAULT_THEMES: LazyLock<hl::ThemeSet> = LazyLock::new(hl::ThemeSet::load_defaults);
static DRACULA: LazyLock<hl::Theme> = LazyLock::new(|| build_theme("Dracula", &DRACULA_COLORS));
static TOKYO_NIGHT: LazyLock<hl::Theme> =
    LazyLock::new(|| build_theme("Tokyo Night", &TOKYO_NIGHT_COLORS));

const LINES_PER_SNAPSHOT: usize = 50;

/// The code color palette of the viewer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodePalette {
    Dracula,
    TokyoNight,
    /// iced's Base16 Mocha, for the other dark themes.
    Mocha,
    /// iced's InspiredGitHub, for light themes.
    GitHub,
}

/// Picks the code palette for the active app theme.
pub fn palette_for(style: ThemeStyle, is_dark: bool) -> CodePalette {
    match style {
        ThemeStyle::Dracula => CodePalette::Dracula,
        ThemeStyle::TokyoNight => CodePalette::TokyoNight,
        ThemeStyle::Violet | ThemeStyle::Default if is_dark => CodePalette::Mocha,
        ThemeStyle::Violet | ThemeStyle::Default => CodePalette::GitHub,
    }
}

impl CodePalette {
    fn theme(self) -> &'static hl::Theme {
        match self {
            Self::Dracula => &DRACULA,
            Self::TokyoNight => &TOKYO_NIGHT,
            Self::Mocha => &DEFAULT_THEMES.themes["base16-mocha.dark"],
            Self::GitHub => &DEFAULT_THEMES.themes["InspiredGitHub"],
        }
    }
}

/// Token colors (0xRRGGBB) of a palette built in code.
struct Colors {
    background: u32,
    foreground: u32,
    comment: u32,
    keyword: u32,
    string: u32,
    function: u32,
    constant: u32,
    kind: u32,
    parameter: u32,
}

const DRACULA_COLORS: Colors = Colors {
    background: 0x282a36,
    foreground: 0xf8f8f2,
    comment: 0x6272a4,
    keyword: 0xff79c6,
    string: 0xf1fa8c,
    function: 0x50fa7b,
    constant: 0xbd93f9,
    kind: 0x8be9fd,
    parameter: 0xffb86c,
};

const TOKYO_NIGHT_COLORS: Colors = Colors {
    background: 0x1a1b26,
    foreground: 0xc0caf5,
    comment: 0x565f89,
    keyword: 0xbb9af7,
    string: 0x9ece6a,
    function: 0x7aa2f7,
    constant: 0xff9e64,
    kind: 0x2ac3de,
    parameter: 0xe0af68,
};

fn sc(hex: u32) -> hl::Color {
    hl::Color { r: (hex >> 16) as u8, g: (hex >> 8) as u8, b: hex as u8, a: 0xff }
}

fn build_theme(name: &str, c: &Colors) -> hl::Theme {
    let plain = FontStyle::empty();
    let rules: [(&str, u32, FontStyle); 14] = [
        ("comment, punctuation.definition.comment", c.comment, plain),
        ("string, punctuation.definition.string", c.string, plain),
        ("constant.numeric, constant.language, constant.character, constant.other", c.constant, plain),
        ("keyword, storage, keyword.operator", c.keyword, plain),
        ("entity.name.tag", c.keyword, plain),
        ("entity.name.function, support.function, meta.function-call entity.name.function", c.function, plain),
        ("entity.other.attribute-name", c.function, plain),
        ("entity.name.type, entity.name.class, entity.name.struct, entity.name.enum, entity.name.trait, entity.other.inherited-class, support.type, support.class", c.kind, FontStyle::ITALIC),
        ("variable.parameter", c.parameter, FontStyle::ITALIC),
        ("markup.heading, markup.heading punctuation.definition.heading", c.constant, FontStyle::BOLD),
        ("markup.bold", c.parameter, FontStyle::BOLD),
        ("markup.italic", c.string, FontStyle::ITALIC),
        ("markup.raw, markup.inline.raw", c.function, plain),
        ("markup.underline.link, string.other.link", c.kind, plain),
    ];
    let scopes = rules
        .iter()
        .filter_map(|(selector, color, font_style)| {
            Some(ThemeItem {
                scope: ScopeSelectors::from_str(selector).ok()?,
                style: StyleModifier {
                    foreground: Some(sc(*color)),
                    background: None,
                    font_style: Some(*font_style),
                },
            })
        })
        .collect();
    hl::Theme {
        name: Some(name.to_string()),
        author: None,
        settings: ThemeSettings {
            foreground: Some(sc(c.foreground)),
            background: Some(sc(c.background)),
            ..ThemeSettings::default()
        },
        scopes,
    }
}

/// Settings of the [`Highlighter`]: the palette and the file extension / language name.
#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    pub palette: CodePalette,
    pub token: String,
}

/// A highlight: the syntect style modifier for a range of the line.
#[derive(Debug)]
pub struct Highlight(StyleModifier);

impl Highlight {
    pub fn color(&self) -> Option<Color> {
        self.0.foreground.map(|c| Color::from_rgba8(c.r, c.g, c.b, c.a as f32 / 255.0))
    }

    pub fn font(&self) -> Option<Font> {
        let style = self.0.font_style?;
        let bold = style.contains(FontStyle::BOLD);
        let italic = style.contains(FontStyle::ITALIC);
        (bold || italic).then_some(Font {
            weight: if bold { font::Weight::Bold } else { font::Weight::Normal },
            style: if italic { font::Style::Italic } else { font::Style::Normal },
            ..Font::MONOSPACE
        })
    }

    /// The text format, for `text_editor(..).highlight_with`.
    pub fn to_format(&self, _theme: &iced::Theme) -> Format<Font> {
        Format { color: self.color(), font: self.font() }
    }
}

fn find_syntax(token: &str) -> &'static SyntaxReference {
    SYNTAXES.find_syntax_by_token(token).unwrap_or_else(|| SYNTAXES.find_syntax_plain_text())
}

/// A syntect highlighter with app-matched palettes.
#[derive(Debug)]
pub struct Highlighter {
    syntax: &'static SyntaxReference,
    highlighter: hl::Highlighter<'static>,
    caches: Vec<(ParseState, ScopeStack)>,
    current_line: usize,
}

impl highlighter::Highlighter for Highlighter {
    type Settings = Settings;
    type Highlight = Highlight;
    type Iterator<'a> = Box<dyn Iterator<Item = (Range<usize>, Highlight)> + 'a>;

    fn new(settings: &Settings) -> Self {
        let syntax = find_syntax(&settings.token);
        Self {
            syntax,
            highlighter: hl::Highlighter::new(settings.palette.theme()),
            caches: vec![(ParseState::new(syntax), ScopeStack::new())],
            current_line: 0,
        }
    }

    fn update(&mut self, settings: &Settings) {
        self.syntax = find_syntax(&settings.token);
        self.highlighter = hl::Highlighter::new(settings.palette.theme());
        self.change_line(0);
    }

    fn change_line(&mut self, line: usize) {
        let snapshot = line / LINES_PER_SNAPSHOT;
        if snapshot <= self.caches.len() {
            self.caches.truncate(snapshot);
            self.current_line = snapshot * LINES_PER_SNAPSHOT;
        } else {
            self.caches.truncate(1);
            self.current_line = 0;
        }
        let state = self
            .caches
            .last()
            .cloned()
            .unwrap_or_else(|| (ParseState::new(self.syntax), ScopeStack::new()));
        self.caches.push(state);
    }

    fn highlight_line(&mut self, line: &str) -> Self::Iterator<'_> {
        if self.current_line / LINES_PER_SNAPSHOT >= self.caches.len() {
            let last = self.caches.last().cloned().expect("caches are never empty");
            self.caches.push(last);
        }
        self.current_line += 1;

        let (parser, stack) = self.caches.last_mut().expect("caches are never empty");
        let ops = parser.parse_line(line, &SYNTAXES).unwrap_or_default();
        let highlighter = &self.highlighter;
        Box::new(scope_ranges(ops, line.len()).filter_map(move |(range, op)| {
            let _ = stack.apply(&op);
            (!range.is_empty())
                .then(|| (range, Highlight(highlighter.style_mod_for_stack(&stack.scopes))))
        }))
    }

    fn current_line(&self) -> usize {
        self.current_line
    }
}

/// Splits a line at the parser's ops: each range comes with the op that starts it.
fn scope_ranges(
    ops: Vec<(usize, ScopeStackOp)>,
    line_length: usize,
) -> impl Iterator<Item = (Range<usize>, ScopeStackOp)> {
    let mut last = 0;
    (0..=ops.len()).map(move |i| {
        let end = ops.get(i).map_or(line_length, |(at, _)| *at);
        let op = if i == 0 { ScopeStackOp::Noop } else { ops[i - 1].1.clone() };
        let range = last..end;
        last = end;
        (range, op)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use iced::advanced::text::Highlighter as _;

    fn rgb(hex: u32) -> Color {
        Color::from_rgb8((hex >> 16) as u8, (hex >> 8) as u8, hex as u8)
    }

    /// Highlights `src` and returns (token text, color, italic) for each colored range.
    fn tokens(palette: CodePalette, token: &str, src: &str) -> Vec<(String, Option<Color>, bool)> {
        let mut h = Highlighter::new(&Settings { palette, token: token.into() });
        let mut out = Vec::new();
        for line in src.lines() {
            for (range, hl) in h.highlight_line(line) {
                let italic = hl.font().is_some_and(|f| f.style == font::Style::Italic);
                out.push((line[range].to_string(), hl.color(), italic));
            }
        }
        out
    }

    fn color_of(toks: &[(String, Option<Color>, bool)], text: &str) -> Option<Color> {
        toks.iter()
            .find(|(t, ..)| t.contains(text))
            .unwrap_or_else(|| panic!("no token {text:?} in {toks:?}"))
            .1
    }

    #[test]
    fn palette_follows_app_theme() {
        assert_eq!(palette_for(ThemeStyle::Dracula, true), CodePalette::Dracula);
        assert_eq!(palette_for(ThemeStyle::TokyoNight, true), CodePalette::TokyoNight);
        assert_eq!(palette_for(ThemeStyle::Violet, true), CodePalette::Mocha);
        assert_eq!(palette_for(ThemeStyle::Default, true), CodePalette::Mocha);
        assert_eq!(palette_for(ThemeStyle::Default, false), CodePalette::GitHub);
    }

    #[test]
    fn dracula_colors_rust() {
        let src = "// note\nfn main() {\n    let s: String = \"hi\";\n    let n = 42;\n    foo(n);\n}\n";
        let t = tokens(CodePalette::Dracula, "rs", src);
        assert_eq!(color_of(&t, "note"), Some(rgb(0x6272a4)));
        assert_eq!(color_of(&t, "fn"), Some(rgb(0xff79c6)));
        assert_eq!(color_of(&t, "let"), Some(rgb(0xff79c6)));
        assert_eq!(color_of(&t, "main"), Some(rgb(0x50fa7b)));
        assert_eq!(color_of(&t, "hi"), Some(rgb(0xf1fa8c)));
        assert_eq!(color_of(&t, "42"), Some(rgb(0xbd93f9)));
        let ty = t.iter().find(|(s, ..)| s == "String").expect("String token");
        assert_eq!(ty.1, Some(rgb(0x8be9fd)));
        assert!(ty.2, "types are italic");
    }

    #[test]
    fn other_languages_and_palettes_highlight() {
        for palette in [CodePalette::Dracula, CodePalette::TokyoNight, CodePalette::Mocha, CodePalette::GitHub] {
            for (token, src) in [
                ("toml", "[package]\nname = \"x\" # c\n"),
                ("json", "{\"a\": 1}\n"),
                ("md", "# Title\n`code`\n"),
                ("sh", "echo \"hi\" # c\n"),
            ] {
                let t = tokens(palette, token, src);
                assert!(t.iter().any(|(_, c, _)| c.is_some()), "{palette:?} {token}: {t:?}");
            }
        }
        let t = tokens(CodePalette::Dracula, "toml", "name = \"x\"\n");
        assert_eq!(color_of(&t, "x"), Some(rgb(0xf1fa8c)));
    }

    #[test]
    fn unknown_syntax_is_plain_text() {
        let t = tokens(CodePalette::Dracula, "no-such-ext", "fn main() {}\n");
        assert!(t.iter().all(|(_, c, _)| c.is_none()), "{t:?}");
        let t = tokens(CodePalette::Dracula, "", "");
        assert!(t.is_empty());
    }

    #[test]
    fn long_file_and_line_changes() {
        let src = "fn a() {}\n".repeat(500);
        let mut h = Highlighter::new(&Settings { palette: CodePalette::Dracula, token: "rs".into() });
        for line in src.lines() {
            let _ = h.highlight_line(line).count();
        }
        assert_eq!(h.current_line(), 500);
        h.change_line(120);
        assert_eq!(h.current_line(), 100);
        let toks: Vec<_> = h.highlight_line("fn b() {}").collect();
        assert_eq!(toks[0].1.color(), Some(rgb(0xff79c6)));
        h.update(&Settings { palette: CodePalette::GitHub, token: "txt".into() });
        assert_eq!(h.current_line(), 0);
    }
}
