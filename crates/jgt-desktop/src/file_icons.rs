//! Colored file-type icons for the IDE file tree and tabs (JGT-50).
//!
//! The icons are our own small 16x16 SVGs in `assets/file-icons/` (style inspired by the
//! VS Code Material Icon Theme). They are embedded in the binary and rasterized once with
//! resvg into [`image::Handle`]s on first use, so nothing is decoded per frame.

use iced::widget::image;
use std::path::Path;
use std::sync::OnceLock;

/// Pixel size the SVGs are rasterized at (2x the 16 px viewBox, sharp on HiDPI).
const RASTER_PX: u32 = 32;

/// One icon of the embedded set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileIcon {
    File,
    Text,
    Rust,
    Toml,
    Json,
    Yaml,
    Markdown,
    Shell,
    Javascript,
    Typescript,
    Python,
    Html,
    Css,
    Image,
    Lock,
    Docker,
    Git,
    License,
    Readme,
    Makefile,
    Folder,
    FolderOpen,
}

impl FileIcon {
    /// Every icon, in a fixed order (the index is used by the handle cache).
    pub const ALL: [FileIcon; 22] = [
        FileIcon::File,
        FileIcon::Text,
        FileIcon::Rust,
        FileIcon::Toml,
        FileIcon::Json,
        FileIcon::Yaml,
        FileIcon::Markdown,
        FileIcon::Shell,
        FileIcon::Javascript,
        FileIcon::Typescript,
        FileIcon::Python,
        FileIcon::Html,
        FileIcon::Css,
        FileIcon::Image,
        FileIcon::Lock,
        FileIcon::Docker,
        FileIcon::Git,
        FileIcon::License,
        FileIcon::Readme,
        FileIcon::Makefile,
        FileIcon::Folder,
        FileIcon::FolderOpen,
    ];

    /// The embedded SVG source of this icon.
    pub fn svg(self) -> &'static [u8] {
        macro_rules! asset {
            ($f:literal) => {
                include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/assets/file-icons/", $f))
            };
        }
        match self {
            FileIcon::File => asset!("file.svg"),
            FileIcon::Text => asset!("text.svg"),
            FileIcon::Rust => asset!("rust.svg"),
            FileIcon::Toml => asset!("toml.svg"),
            FileIcon::Json => asset!("json.svg"),
            FileIcon::Yaml => asset!("yaml.svg"),
            FileIcon::Markdown => asset!("markdown.svg"),
            FileIcon::Shell => asset!("shell.svg"),
            FileIcon::Javascript => asset!("javascript.svg"),
            FileIcon::Typescript => asset!("typescript.svg"),
            FileIcon::Python => asset!("python.svg"),
            FileIcon::Html => asset!("html.svg"),
            FileIcon::Css => asset!("css.svg"),
            FileIcon::Image => asset!("image.svg"),
            FileIcon::Lock => asset!("lock.svg"),
            FileIcon::Docker => asset!("docker.svg"),
            FileIcon::Git => asset!("git.svg"),
            FileIcon::License => asset!("license.svg"),
            FileIcon::Readme => asset!("readme.svg"),
            FileIcon::Makefile => asset!("makefile.svg"),
            FileIcon::Folder => asset!("folder.svg"),
            FileIcon::FolderOpen => asset!("folder-open.svg"),
        }
    }

    fn index(self) -> usize {
        Self::ALL.iter().position(|i| *i == self).unwrap_or(0)
    }

    /// The image handle of this icon, rasterized once for the whole process.
    pub fn handle(self) -> image::Handle {
        static HANDLES: OnceLock<Vec<image::Handle>> = OnceLock::new();
        HANDLES
            .get_or_init(|| {
                FileIcon::ALL
                    .iter()
                    .map(|icon| {
                        let (w, h, rgba) = rasterize(icon.svg(), RASTER_PX)
                            .unwrap_or_else(|| (1, 1, vec![0; 4]));
                        image::Handle::from_rgba(w, h, rgba)
                    })
                    .collect()
            })[self.index()]
            .clone()
    }
}

/// Rasterizes an SVG into straight (non-premultiplied) RGBA pixels of `px` x `px`.
/// Returns `None` when the SVG does not parse.
pub fn rasterize(svg: &[u8], px: u32) -> Option<(u32, u32, Vec<u8>)> {
    use resvg::{tiny_skia, usvg};
    let tree = usvg::Tree::from_data(svg, &usvg::Options::default()).ok()?;
    let mut pixmap = tiny_skia::Pixmap::new(px, px)?;
    let size = tree.size();
    let scale = px as f32 / size.width().max(size.height());
    resvg::render(&tree, tiny_skia::Transform::from_scale(scale, scale), &mut pixmap.as_mut());
    let rgba = pixmap
        .pixels()
        .iter()
        .flat_map(|p| {
            let c = p.demultiply();
            [c.red(), c.green(), c.blue(), c.alpha()]
        })
        .collect();
    Some((px, px, rgba))
}

/// Picks the icon of a file by its name: exact (case-insensitive) names first, then the extension.
pub fn icon_for_file(name: &str) -> FileIcon {
    let lower = name.to_ascii_lowercase();
    match lower.as_str() {
        "cargo.lock" => return FileIcon::Lock,
        "dockerfile" | "containerfile" | ".dockerignore" => return FileIcon::Docker,
        ".gitignore" | ".gitattributes" | ".gitmodules" | ".gitkeep" => return FileIcon::Git,
        "makefile" | "gnumakefile" => return FileIcon::Makefile,
        ".bashrc" | ".zshrc" | ".profile" | ".bash_profile" => return FileIcon::Shell,
        _ => {}
    }
    if lower.starts_with("license") || lower.starts_with("licence") || lower.starts_with("copying") {
        return FileIcon::License;
    }
    if lower.starts_with("readme") {
        return FileIcon::Readme;
    }
    if lower.starts_with("dockerfile.") {
        return FileIcon::Docker;
    }
    let ext = Path::new(&lower)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");
    match ext {
        "rs" => FileIcon::Rust,
        "toml" => FileIcon::Toml,
        "json" | "jsonc" | "json5" => FileIcon::Json,
        "yaml" | "yml" => FileIcon::Yaml,
        "md" | "markdown" => FileIcon::Markdown,
        "sh" | "bash" | "zsh" | "fish" => FileIcon::Shell,
        "js" | "mjs" | "cjs" | "jsx" => FileIcon::Javascript,
        "ts" | "mts" | "cts" | "tsx" => FileIcon::Typescript,
        "py" | "pyi" => FileIcon::Python,
        "html" | "htm" => FileIcon::Html,
        "css" | "scss" => FileIcon::Css,
        "png" | "jpg" | "jpeg" | "gif" | "svg" | "webp" | "ico" | "bmp" => FileIcon::Image,
        "lock" => FileIcon::Lock,
        "dockerfile" => FileIcon::Docker,
        "txt" | "log" => FileIcon::Text,
        _ => FileIcon::File,
    }
}

/// Picks the icon of a folder. The name is reserved for special folder icons (not drawn yet).
pub fn icon_for_folder(_name: &str, open: bool) -> FileIcon {
    if open {
        FileIcon::FolderOpen
    } else {
        FileIcon::Folder
    }
}

/// The icon widget (square, `size` px) for a row or tab.
pub fn view(icon: FileIcon, size: f32) -> image::Image<image::Handle> {
    image(icon.handle()).width(size).height(size)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_names_win_over_extensions() {
        assert_eq!(icon_for_file("Cargo.lock"), FileIcon::Lock);
        assert_eq!(icon_for_file("Cargo.toml"), FileIcon::Toml);
        assert_eq!(icon_for_file("Dockerfile"), FileIcon::Docker);
        assert_eq!(icon_for_file("Dockerfile.dev"), FileIcon::Docker);
        assert_eq!(icon_for_file(".gitignore"), FileIcon::Git);
        assert_eq!(icon_for_file("LICENSE"), FileIcon::License);
        assert_eq!(icon_for_file("LICENSE-MIT"), FileIcon::License);
        assert_eq!(icon_for_file("LICENSE-material-symbols.txt"), FileIcon::License);
        assert_eq!(icon_for_file("README.md"), FileIcon::Readme);
        assert_eq!(icon_for_file("readme"), FileIcon::Readme);
        assert_eq!(icon_for_file("Makefile"), FileIcon::Makefile);
    }

    #[test]
    fn extensions_map_case_insensitively() {
        for (name, icon) in [
            ("main.rs", FileIcon::Rust),
            ("MAIN.RS", FileIcon::Rust),
            ("a.toml", FileIcon::Toml),
            ("a.json", FileIcon::Json),
            ("CHANGELOG.md", FileIcon::Markdown),
            ("x.sh", FileIcon::Shell),
            ("x.bash", FileIcon::Shell),
            ("x.zsh", FileIcon::Shell),
            ("ci.yaml", FileIcon::Yaml),
            ("ci.YML", FileIcon::Yaml),
            ("a.js", FileIcon::Javascript),
            ("a.mjs", FileIcon::Javascript),
            ("a.ts", FileIcon::Typescript),
            ("a.py", FileIcon::Python),
            ("index.html", FileIcon::Html),
            ("a.css", FileIcon::Css),
            ("a.PNG", FileIcon::Image),
            ("a.jpg", FileIcon::Image),
            ("a.jpeg", FileIcon::Image),
            ("a.gif", FileIcon::Image),
            ("a.svg", FileIcon::Image),
            ("a.webp", FileIcon::Image),
            ("package-lock.lock", FileIcon::Lock),
            ("notes.txt", FileIcon::Text),
            ("archive.tar.gz", FileIcon::File),
        ] {
            assert_eq!(icon_for_file(name), icon, "{name}");
        }
    }

    #[test]
    fn odd_names_fall_back_to_generic() {
        for name in ["", ".", "..", ".env", ".env.local", "noext", "weird.", "a.unknownext", "é.ü", "日本"] {
            assert_eq!(icon_for_file(name), FileIcon::File, "{name:?}");
        }
        assert_eq!(icon_for_file(".bashrc"), FileIcon::Shell);
    }

    #[test]
    fn folders_open_and_closed() {
        assert_eq!(icon_for_folder("src", false), FileIcon::Folder);
        assert_eq!(icon_for_folder("src", true), FileIcon::FolderOpen);
        assert_eq!(icon_for_folder("", true), FileIcon::FolderOpen);
    }

    #[test]
    fn every_icon_embeds_parses_and_draws() {
        for icon in FileIcon::ALL {
            let svg = icon.svg();
            assert!(!svg.is_empty(), "{icon:?}");
            let (w, h, rgba) = rasterize(svg, RASTER_PX).unwrap_or_else(|| panic!("{icon:?} does not parse"));
            assert_eq!((w, h), (RASTER_PX, RASTER_PX));
            assert_eq!(rgba.len(), (w * h * 4) as usize);
            assert!(rgba.chunks(4).any(|p| p[3] > 0), "{icon:?} draws nothing");
        }
    }

    #[test]
    fn handles_are_created_once() {
        let a = FileIcon::Rust.handle();
        let b = FileIcon::Rust.handle();
        assert_eq!(a.id(), b.id());
        assert_ne!(FileIcon::Rust.handle().id(), FileIcon::Toml.handle().id());
    }
}
