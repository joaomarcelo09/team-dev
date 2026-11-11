//! IDE tab view: read-only file browser with git changes (P5).

use crate::app::{App, Msg, Viewer};
use crate::icons;
use crate::ide::{ChangeStatus, DiffLine};
use crate::style;
use iced::widget::{
    button, checkbox, column, container, mouse_area, pick_list, row, rule, scrollable, space,
    text, text_editor, tooltip,
};
use iced::{Alignment, Border, Color, Element, Font, Length::Fill, Length, Theme};

/// Maps a change status to its corresponding theme palette color.
pub fn status_color(status: ChangeStatus, theme: &Theme) -> Color {
    match status {
        ChangeStatus::Modified => theme.palette().warning,
        ChangeStatus::Added => theme.palette().success,
        ChangeStatus::Deleted => theme.palette().danger,
        ChangeStatus::Renamed => theme.palette().primary,
    }
}

/// Generates the line-number gutter text for a given line count, right-aligned with monospace spacing.
pub fn build_gutter_text(line_count: usize) -> String {
    if line_count == 0 {
        return String::new();
    }
    let digits = line_count.to_string().len().max(2);
    let mut s = String::new();
    for n in 1..=line_count {
        use std::fmt::Write;
        if n > 1 {
            s.push('\n');
        }
        let _ = write!(s, "{:>digits$}", n);
    }
    s
}

/// Builds the top bar with Source selector, Changed only checkbox, and Refresh button.
fn top_bar<'a>(app: &'a App) -> Element<'a, Msg> {
    let source_label = text("Source:")
        .size(style::fs(13))
        .style(|t: &Theme| text::Style {
            color: Some(style::muted(t)),
        });

    let selected_source = if app.ide.sources.is_empty() {
        None
    } else {
        Some(app.ide.source.clone())
    };

    let source_picker = pick_list(
        &app.ide.sources[..],
        selected_source,
        Msg::IdeSelectSource,
    )
    .text_size(style::fs(13))
    .padding([4, 10])
    .style(style::workspace_selector)
    .menu_style(style::workspace_menu);

    let is_main = matches!(app.ide.source.kind, crate::ide::SourceKind::Main);
    let changed_cb = checkbox(app.ide.changed_only && !is_main)
        .label("Changed only")
        .size(style::fs(14))
        .text_size(style::fs(13))
        .on_toggle_maybe(if is_main { None } else { Some(Msg::IdeChangedOnly) });

    let refresh_btn = button(text("Refresh").size(style::fs(13)))
        .style(style::ghost)
        .padding([4, 10])
        .on_press(Msg::IdeRefresh);

    row![
        source_label,
        source_picker,
        changed_cb,
        refresh_btn,
    ]
    .spacing(12)
    .align_y(Alignment::Center)
    .padding([8, 12])
    .into()
}

/// Builds the file and directory picker (right pane).
fn picker_pane<'a>(app: &'a App, theme: &Theme) -> Element<'a, Msg> {
    let mut picker_col = column![].spacing(2).width(Fill);

    if let Some(err) = &app.ide.error {
        let err_banner = container(
            row![
                icons::icon(icons::Icon::Warning, style::fs(14))
                    .style(move |t: &Theme| text::Style {
                        color: Some(t.palette().danger),
                    }),
                text(err)
                    .size(style::fs(12))
                    .style(move |t: &Theme| text::Style {
                        color: Some(t.palette().danger),
                    }),
            ]
            .spacing(6)
            .align_y(Alignment::Center),
        )
        .padding([6, 10])
        .width(Fill)
        .style(|t: &Theme| container::Style {
            background: Some(t.palette().danger.scale_alpha(0.15).into()),
            border: Border {
                color: t.palette().danger.scale_alpha(0.4),
                width: 1.0,
                radius: style::RADIUS_SM.into(),
            },
            ..Default::default()
        });
        picker_col = picker_col.push(err_banner);
    }

    if let Some(listing) = &app.ide.listing {
        if listing.truncated {
            let trunc_banner = container(
                text(format!("Listing truncated ({} files)", listing.total_count))
                    .size(style::fs(12))
                    .style(move |t: &Theme| text::Style {
                        color: Some(t.palette().warning),
                    }),
            )
            .padding([6, 10])
            .width(Fill)
            .style(|t: &Theme| container::Style {
                background: Some(t.palette().warning.scale_alpha(0.15).into()),
                border: Border {
                    color: t.palette().warning.scale_alpha(0.4),
                    width: 1.0,
                    radius: style::RADIUS_SM.into(),
                },
                ..Default::default()
            });
            picker_col = picker_col.push(trunc_banner);
        }

        let is_main = matches!(app.ide.source.kind, crate::ide::SourceKind::Main);
        let rows = crate::ide::visible_rows(
            &listing.tree,
            &app.ide.open_dirs,
            app.ide.changed_only && !is_main,
        );
        if rows.is_empty() {
            let msg = if app.ide.changed_only {
                "No changed files"
            } else {
                "No files"
            };
            let empty_text = container(
                text(msg)
                    .size(style::fs(13))
                    .style(|t: &Theme| text::Style {
                        color: Some(style::muted(t)),
                    }),
            )
            .padding([12, 10]);
            picker_col = picker_col.push(empty_text);
        } else {
            for row in rows {
                let indent_space = space().width((row.depth as f32) * 14.0);
                if row.is_dir {
                    let chevron_icon = if row.is_open {
                        icons::Icon::ExpandMore
                    } else {
                        icons::Icon::ChevronRight
                    };
                    let chevron = icons::icon(chevron_icon, style::fs(14))
                        .style(|t: &Theme| text::Style {
                            color: Some(style::muted(t)),
                        });
                    let folder_content = row![
                        indent_space,
                        chevron,
                        text(row.name)
                            .size(style::fs(13))
                            .wrapping(text::Wrapping::None),
                    ]
                    .spacing(4)
                    .align_y(Alignment::Center);

                    let folder_btn = button(folder_content)
                        .width(Fill)
                        .padding([3, 6])
                        .style(style::ghost)
                        .on_press(Msg::IdeToggleDir(row.path));
                    picker_col = picker_col.push(folder_btn);
                } else {
                    let is_selected = app.ide.selected.as_deref() == Some(&row.path);
                    let mut file_content = row![
                        indent_space,
                        space().width(14.0),
                        text(row.name)
                            .size(style::fs(13))
                            .wrapping(text::Wrapping::None),
                    ]
                    .spacing(4)
                    .align_y(Alignment::Center);

                    if row.is_secret {
                        let lock = icons::icon(icons::Icon::Lock, style::fs(11));
                        file_content = file_content.push(lock);
                    }

                    file_content = file_content.push(space::horizontal());

                    if let Some(st) = row.status {
                        let color = status_color(st, theme);
                        let badge_txt = text(st.badge())
                            .size(style::fs(11))
                            .font(Font::MONOSPACE)
                            .style(move |_| text::Style {
                                color: Some(color),
                            });
                        let badge_pill = container(badge_txt)
                            .padding([1, 4])
                            .style(move |_| container::Style {
                                background: Some(color.scale_alpha(0.15).into()),
                                border: Border {
                                    color: color.scale_alpha(0.4),
                                    width: 1.0,
                                    radius: style::RADIUS_SM.into(),
                                },
                                ..Default::default()
                            });
                        file_content = file_content.push(badge_pill);
                    }

                    let file_btn = button(file_content)
                        .width(Fill)
                        .padding([3, 6])
                        .style(style::session_item(is_selected))
                        .on_press(Msg::IdeOpenFile(row.path));
                    picker_col = picker_col.push(file_btn);
                }
            }
        }
    } else if app.ide.error.is_none() {
        let loading_text = container(
            text("Loading files...")
                .size(style::fs(13))
                .style(|t: &Theme| text::Style {
                    color: Some(style::muted(t)),
                }),
        )
        .padding([12, 10]);
        picker_col = picker_col.push(loading_text);
    }

    let picker_scroll = scrollable(container(picker_col).padding([4, 6]))
        .height(Fill)
        .width(Fill);

    container(picker_scroll)
        .width(Length::Fixed(320.0))
        .height(Fill)
        .into()
}

/// Builds the tab bar of open files above the viewer, or `None` when no tab is open.
fn tab_bar<'a>(app: &'a App) -> Option<Element<'a, Msg>> {
    if app.ide.tabs.tabs.is_empty() {
        return None;
    }
    let mut tabs_row = row![].spacing(2).align_y(Alignment::Center);
    for (i, tab) in app.ide.tabs.tabs.iter().enumerate() {
        let active = app.ide.tabs.active == Some(i);
        let close_btn = button(icons::icon(icons::Icon::Close, style::fs(12)))
            .style(style::ghost)
            .padding([0, 2])
            .on_press(Msg::IdeTabClose(i));
        let content = row![
            text(tab.name())
                .size(style::fs(13))
                .wrapping(text::Wrapping::None),
            close_btn,
        ]
        .spacing(6)
        .align_y(Alignment::Center);
        let tab_btn = button(content)
            .style(style::tab(active))
            .padding([3, 4])
            .on_press(Msg::IdeTabFocus(i));
        let tip_label = if tab.source.root == app.ide.source.root {
            tab.path.clone()
        } else {
            format!("{} ({})", tab.path, tab.source.label)
        };
        let with_tip = tooltip(
            mouse_area(tab_btn).on_middle_press(Msg::IdeTabClose(i)),
            container(text(tip_label).size(style::fs(12)))
                .padding([6, 10])
                .style(container::rounded_box),
            tooltip::Position::Bottom,
        )
        .gap(4);
        tabs_row = tabs_row.push(with_tip);
    }
    let bar = scrollable(container(tabs_row).padding([4, 8]))
        .direction(scrollable::Direction::Horizontal(
            scrollable::Scrollbar::new().width(4).scroller_width(4),
        ))
        .width(Fill);
    Some(column![bar, rule::horizontal(1).style(rule::weak)].into())
}

/// Builds the viewer area (left pane).
fn viewer_pane<'a>(app: &'a App, theme: &Theme) -> Element<'a, Msg> {
    let selected_status = app.ide.selected.as_ref().and_then(|sel_path| {
        app.ide
            .listing
            .as_ref()
            .and_then(|l| l.entries.iter().find(|e| &e.path == sel_path).and_then(|e| e.status))
    });

    let header_bar: Option<Element<'a, Msg>> = if let Some(path) = &app.ide.selected {
        let mut header_row = row![
            text(path.clone())
                .size(style::fs(13))
                .font(Font::MONOSPACE)
                .wrapping(text::Wrapping::None),
        ]
        .spacing(8)
        .align_y(Alignment::Center);

        if selected_status.is_some() {
            header_row = header_row.push(space::horizontal());
            let toggle = row![
                button(text("File").size(style::fs(12)))
                    .style(style::segment_button(!app.ide.show_diff))
                    .padding([3, 8])
                    .on_press(Msg::IdeShowDiff(false)),
                button(text("Diff").size(style::fs(12)))
                    .style(style::segment_button(app.ide.show_diff))
                    .padding([3, 8])
                    .on_press(Msg::IdeShowDiff(true)),
            ]
            .spacing(2);
            header_row = header_row.push(toggle);
        }

        Some(
            column![
                container(header_row).padding([6, 12]).width(Fill),
                rule::horizontal(1).style(rule::weak),
            ]
            .into(),
        )
    } else {
        None
    };

    let viewer_content: Element<'a, Msg> = match &app.ide.viewer {
        Viewer::Loading => container(
            text("Loading...")
                .size(style::fs(14))
                .style(|t: &Theme| text::Style {
                    color: Some(style::muted(t)),
                }),
        )
        .center(Fill)
        .into(),
        Viewer::Notice(msg) => container(
            text(msg.clone())
                .size(style::fs(14))
                .style(|t: &Theme| text::Style {
                    color: Some(style::muted(t)),
                }),
        )
        .center(Fill)
        .into(),
        Viewer::Text(content, ext) => {
            let line_count = content.line_count();
            let gutter_str = build_gutter_text(line_count);
            let gutter_text = text(gutter_str)
                .font(Font::MONOSPACE)
                .size(style::fs(13))
                .line_height(text::LineHeight::Relative(1.3))
                .style(|t: &Theme| text::Style {
                    color: Some(style::muted(t)),
                });

            let gutter_container = container(gutter_text)
                .padding([8, 10])
                .style(|t: &Theme| {
                    let p = t.extended_palette();
                    container::Style {
                        background: Some(p.background.weak.color.scale_alpha(0.3).into()),
                        border: Border {
                            color: p.background.strong.color.scale_alpha(0.3),
                            width: 1.0,
                            radius: 0.0.into(),
                        },
                        ..Default::default()
                    }
                });

            let hl_theme = if theme.extended_palette().is_dark {
                iced::highlighter::Theme::Base16Mocha
            } else {
                iced::highlighter::Theme::InspiredGitHub
            };

            let editor: Element<'a, Msg> = text_editor(content)
                .highlight(ext, hl_theme)
                .on_action(Msg::IdeViewer)
                .padding([8, 10])
                .size(style::fs(13))
                .font(Font::MONOSPACE)
                .line_height(text::LineHeight::Relative(1.3))
                .wrapping(text::Wrapping::None)
                .height(Length::Shrink)
                .style(style::read_only_editor)
                .into();

            let code_row = row![gutter_container, editor].height(Length::Shrink);

            scrollable(code_row)
                .direction(scrollable::Direction::Both {
                    vertical: scrollable::Scrollbar::default(),
                    horizontal: scrollable::Scrollbar::default(),
                })
                .width(Fill)
                .height(Fill)
                .into()
        }
        Viewer::Diff(lines) => {
            if lines.is_empty() {
                container(
                    text("No changes")
                        .size(style::fs(14))
                        .style(|t: &Theme| text::Style {
                            color: Some(style::muted(t)),
                        }),
                )
                .center(Fill)
                .into()
            } else {
                let palette = theme.palette();
                let mut diff_col = column![].spacing(0);
                for dl in lines {
                    let (color, line_text) = match dl {
                        DiffLine::Add(s) => (Some(palette.success), s),
                        DiffLine::Del(s) => (Some(palette.danger), s),
                        DiffLine::Hunk(s) => (Some(style::muted(theme)), s),
                        DiffLine::Meta(s) => (Some(style::muted(theme)), s),
                        DiffLine::Ctx(s) => (None, s),
                    };
                    let t = text(line_text)
                        .font(Font::MONOSPACE)
                        .size(style::fs(13))
                        .line_height(text::LineHeight::Relative(1.3))
                        .wrapping(text::Wrapping::None);
                    let styled = if let Some(c) = color {
                        t.style(move |_| text::Style { color: Some(c) })
                    } else {
                        t
                    };
                    diff_col = diff_col.push(styled);
                }

                scrollable(container(diff_col).padding([8, 10]))
                    .direction(scrollable::Direction::Both {
                        vertical: scrollable::Scrollbar::default(),
                        horizontal: scrollable::Scrollbar::default(),
                    })
                    .width(Fill)
                    .height(Fill)
                    .into()
            }
        }
    };

    let mut viewer_layout = column![].width(Fill).height(Fill);
    if let Some(bar) = tab_bar(app) {
        viewer_layout = viewer_layout.push(bar);
    }
    if let Some(header) = header_bar {
        viewer_layout = viewer_layout.push(header);
    }
    viewer_layout = viewer_layout.push(viewer_content);

    viewer_layout.into()
}

/// The main entry point for the IDE tab view.
pub fn ide<'a>(app: &'a App) -> Element<'a, Msg> {
    let theme = app.theme();
    let top = top_bar(app);
    let body = row![
        viewer_pane(app, &theme),
        rule::vertical(1).style(rule::weak),
        picker_pane(app, &theme),
    ]
    .width(Fill)
    .height(Fill);

    column![top, rule::horizontal(1).style(rule::weak), body]
        .width(Fill)
        .height(Fill)
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{Listing, Viewer};
    use crate::ide::{FileEntry, Source, SourceKind};
    use std::path::PathBuf;

    #[test]
    fn test_build_gutter_text() {
        assert_eq!(build_gutter_text(0), "");
        assert_eq!(build_gutter_text(1), " 1");
        assert_eq!(build_gutter_text(3), " 1\n 2\n 3");
        let g10 = build_gutter_text(10);
        let lines: Vec<&str> = g10.lines().collect();
        assert_eq!(lines.len(), 10);
        assert_eq!(lines[0], " 1");
        assert_eq!(lines[8], " 9");
        assert_eq!(lines[9], "10");
    }

    #[test]
    fn test_status_color_mapping() {
        let theme = style::dark();
        assert_eq!(
            status_color(ChangeStatus::Modified, &theme),
            theme.palette().warning
        );
        assert_eq!(
            status_color(ChangeStatus::Added, &theme),
            theme.palette().success
        );
        assert_eq!(
            status_color(ChangeStatus::Deleted, &theme),
            theme.palette().danger
        );
        assert_eq!(
            status_color(ChangeStatus::Renamed, &theme),
            theme.palette().primary
        );
    }

    #[test]
    fn test_source_display() {
        let s = Source {
            label: "jgt-34".to_string(),
            root: PathBuf::from("/worktree/jgt-34"),
            kind: SourceKind::Worktree {
                branch: "jgt/jgt-34".to_string(),
            },
        };
        assert_eq!(format!("{s}"), "jgt-34");
    }

    #[test]
    fn test_ide_view_renders_all_viewer_states() {
        let (mut app, _) = App::new(None);
        let wt = Source {
            label: "jgt-34".to_string(),
            root: PathBuf::from("/worktree/jgt-34"),
            kind: SourceKind::Worktree {
                branch: "jgt/jgt-34".to_string(),
            },
        };
        app.ide.sources = vec![wt.clone()];
        app.ide.source = wt;

        let entries = vec![
            FileEntry::new("src/main.rs", Some(ChangeStatus::Modified)),
            FileEntry::new("src/.env", None),
            FileEntry::new("README.md", None),
        ];
        let len = entries.len();
        let listing = Listing::new(entries, false, len);
        app.ide.listing = Some(listing);
        app.ide.open_dirs.insert("src".to_string());

        // 1. Notice viewer state
        app.ide.viewer = Viewer::Notice("Select a file to view".to_string());
        let _ = ide(&app);

        // 2. Loading viewer state
        app.ide.selected = Some("src/main.rs".to_string());
        app.ide.viewer = Viewer::Loading;
        let _ = ide(&app);

        // 3. Text viewer state with changes (shows File | Diff toggle in header)
        app.ide.viewer = Viewer::Text(
            text_editor::Content::with_text("fn main() {\n    println!(\"hello\");\n}\n"),
            "rs".to_string(),
        );
        let _ = ide(&app);

        // 4. Diff viewer state with DiffLine variants
        app.ide.show_diff = true;
        app.ide.viewer = Viewer::Diff(vec![
            DiffLine::Meta("--- a/src/main.rs".to_string()),
            DiffLine::Meta("+++ b/src/main.rs".to_string()),
            DiffLine::Hunk("@@ -1,3 +1,3 @@".to_string()),
            DiffLine::Ctx(" fn main() {".to_string()),
            DiffLine::Del("-    old();".to_string()),
            DiffLine::Add("+    new();".to_string()),
            DiffLine::Ctx(" }".to_string()),
        ]);
        let _ = ide(&app);

        // 5. Empty diff
        app.ide.viewer = Viewer::Diff(vec![]);
        let _ = ide(&app);

        // 6. Error state & truncated banner
        app.ide.error = Some("Failed to load git diff".to_string());
        if let Some(l) = &mut app.ide.listing {
            l.truncated = true;
        }
        let _ = ide(&app);

        // 7. Changed only filter
        app.ide.changed_only = true;
        let _ = ide(&app);

        // 8. Tab bar with a tab of the current source and one of another source
        let main = Source {
            label: "Main".to_string(),
            root: PathBuf::from("/project"),
            kind: SourceKind::Main,
        };
        app.ide.tabs.open(crate::ide_tabs::IdeTab::new(main, "README.md", false));
        app.ide.tabs.open(crate::ide_tabs::IdeTab::new(app.ide.source.clone(), "src/main.rs", true));
        let _ = ide(&app);
    }
}
