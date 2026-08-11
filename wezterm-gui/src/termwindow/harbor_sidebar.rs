use crate::termwindow::box_model::*;
use crate::termwindow::UIItemType;
use crate::utilsprites::RenderMetrics;
use crate::{harbor_mobile, harbor_workspace};
use config::{Dimension, DimensionContext};
use mux::Mux;
use std::path::PathBuf;
use termwiz::cell::unicode_column_width;
use window::color::LinearRgba;

fn new_workspace_directory(home: Option<PathBuf>) -> anyhow::Result<PathBuf> {
    let home = home.ok_or_else(|| anyhow::anyhow!("unable to resolve the user home directory"))?;
    if !home.is_dir() {
        anyhow::bail!("the user home directory does not exist: {}", home.display());
    }
    Ok(home)
}

/// Soft-wrap `text` so each visual line fits roughly `max_cols` cells.
/// Existing newlines are preserved; long tokens are hard-broken.
fn wrap_sidebar_lines(text: &str, max_cols: usize) -> Vec<String> {
    let max_cols = max_cols.max(8);
    let mut lines = Vec::new();
    for paragraph in text.split('\n') {
        if paragraph.is_empty() {
            lines.push(String::new());
            continue;
        }
        let mut current = String::new();
        for word in paragraph.split_whitespace() {
            if word.chars().count() > max_cols {
                if !current.is_empty() {
                    lines.push(std::mem::take(&mut current));
                }
                let mut chunk = String::new();
                for ch in word.chars() {
                    if chunk.chars().count() >= max_cols {
                        lines.push(std::mem::take(&mut chunk));
                    }
                    chunk.push(ch);
                }
                if !chunk.is_empty() {
                    current = chunk;
                }
                continue;
            }
            let next_len = if current.is_empty() {
                word.chars().count()
            } else {
                current.chars().count() + 1 + word.chars().count()
            };
            if next_len > max_cols && !current.is_empty() {
                lines.push(std::mem::take(&mut current));
                current.push_str(word);
            } else {
                if !current.is_empty() {
                    current.push(' ');
                }
                current.push_str(word);
            }
        }
        if !current.is_empty() {
            lines.push(current);
        }
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

/// Keep a sidebar line to exactly one visual row; overflow becomes an ellipsis.
/// Width is measured in cells so CJK text, which is double-width, is not
/// allowed to overflow the panel.
fn truncate_line(text: &str, max_cols: usize) -> String {
    let max_cols = max_cols.max(2);
    let flattened = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if unicode_column_width(&flattened, None) <= max_cols {
        return flattened;
    }
    let budget = max_cols - 1; // room for the ellipsis
    let mut out = String::new();
    let mut width = 0;
    for ch in flattened.chars() {
        let ch_width = unicode_column_width(ch.encode_utf8(&mut [0u8; 4]), None);
        if width + ch_width > budget {
            break;
        }
        width += ch_width;
        out.push(ch);
    }
    out.push('…');
    out
}

/// Detail lines for a row: the agent name, then its one-line task summary.
/// Returns `None` when no AI agent is running, which keeps the row down to its
/// directory line.
fn workspace_detail(agent: Option<&str>, summary: Option<&str>, max_cols: usize) -> Option<String> {
    let agent = agent.map(str::trim).filter(|value| !value.is_empty())?;
    let mut detail = format!("  {}", truncate_line(agent, max_cols));
    if let Some(summary) = summary.map(str::trim).filter(|value| !value.is_empty()) {
        detail.push_str("\n  ");
        detail.push_str(&truncate_line(summary, max_cols));
    }
    Some(detail)
}

/// box_model Text is single-line; multi-line content must be Children of Text rows.
fn wrapped_text_element(
    font: &std::rc::Rc<wezterm_font::LoadedFont>,
    text: &str,
    max_cols: usize,
    colors: ElementColors,
) -> Element {
    let lines = wrap_sidebar_lines(text, max_cols);
    if lines.len() == 1 {
        return Element::new(
            font,
            ElementContent::Text(lines.into_iter().next().unwrap()),
        )
        .display(DisplayType::Block)
        .colors(colors);
    }
    let kids: Vec<_> = lines
        .into_iter()
        .map(|line| {
            Element::new(font, ElementContent::Text(line))
                .display(DisplayType::Block)
                .colors(ElementColors {
                    border: BorderColor::default(),
                    bg: InheritableColor::Inherited,
                    text: InheritableColor::Inherited,
                })
        })
        .collect();
    Element::new(font, ElementContent::Children(kids))
        .display(DisplayType::Block)
        .colors(colors)
}

impl crate::TermWindow {
    pub fn harbor_sidebar_width(&self) -> usize {
        let min_window = harbor_workspace::SIDEBAR_DEFAULT_WIDTH + 320;
        if !harbor_workspace::sidebar_visible() || self.dimensions.pixel_width < min_window {
            0
        } else {
            harbor_workspace::sidebar_width().min(self.dimensions.pixel_width.saturating_sub(320))
        }
    }

    pub fn invalidate_harbor_sidebar(&mut self) {
        self.harbor_sidebar.take();
    }

    fn sidebar_colors(selected: bool) -> (ElementColors, Option<ElementColors>) {
        let navy = LinearRgba(0.018, 0.027, 0.045, 0.97);
        let selected_bg = LinearRgba(0.025, 0.16, 0.18, 0.96);
        let hover_bg = LinearRgba(0.035, 0.09, 0.12, 0.96);
        let teal = LinearRgba(0.12, 0.72, 0.64, 1.0);
        let text = LinearRgba(0.84, 0.89, 0.91, 1.0);
        (
            ElementColors {
                border: BorderColor::default(),
                bg: if selected { selected_bg } else { navy }.into(),
                text: if selected { teal } else { text }.into(),
            },
            Some(ElementColors {
                border: BorderColor::default(),
                bg: hover_bg.into(),
                text: teal.into(),
            }),
        )
    }

    fn build_harbor_sidebar(&self) -> anyhow::Result<ComputedElement> {
        let width = self.harbor_sidebar_width() as f32;
        let border = self.get_os_border();
        let height =
            self.dimensions.pixel_height as f32 - (border.top + border.bottom).get() as f32;
        let content_width = (width - 28.).max(1.);
        let font = self.fonts.default_font()?;
        let metrics = RenderMetrics::with_font_metrics(&font.metrics());
        let cell_w = (metrics.cell_size.width as f32).max(1.);
        let max_cols = ((content_width - 16.) / cell_w).floor() as usize;
        let navy = LinearRgba(0.018, 0.027, 0.045, 0.97);
        let teal = LinearRgba(0.12, 0.72, 0.64, 1.0);
        let muted = LinearRgba(0.42, 0.5, 0.54, 1.0);
        let mut children = vec![];

        children.push(
            Element::new(&font, ElementContent::Text("Terminal Harbor".to_string()))
                .display(DisplayType::Block)
                .line_height(Some(1.8))
                .padding(BoxDimension {
                    left: Dimension::Pixels(16.),
                    right: Dimension::Pixels(12.),
                    top: Dimension::Pixels(10.),
                    bottom: Dimension::Pixels(6.),
                })
                .min_width(Some(Dimension::Pixels(content_width)))
                .max_width(Some(Dimension::Pixels(content_width)))
                .colors(ElementColors {
                    border: BorderColor::default(),
                    bg: navy.into(),
                    text: teal.into(),
                }),
        );

        children.push(
            Element::new(&font, ElementContent::Text("＋  New workspace".to_string()))
                .display(DisplayType::Block)
                .item_type(UIItemType::HarborAddWorkspace)
                .line_height(Some(1.55))
                .padding(BoxDimension {
                    left: Dimension::Pixels(16.),
                    right: Dimension::Pixels(12.),
                    top: Dimension::Pixels(4.),
                    bottom: Dimension::Pixels(5.),
                })
                .min_width(Some(Dimension::Pixels(content_width)))
                .max_width(Some(Dimension::Pixels(content_width)))
                .colors(ElementColors {
                    border: BorderColor::default(),
                    bg: navy.into(),
                    text: muted.into(),
                })
                .hover_colors(Some(ElementColors {
                    border: BorderColor::default(),
                    bg: LinearRgba(0.035, 0.09, 0.12, 0.96).into(),
                    text: teal.into(),
                })),
        );

        let pair_label = if harbor_mobile::pairing_ui_visible() {
            "▾  Hide mobile pairing"
        } else {
            "📱  Pair mobile"
        };
        let pair_colors = ElementColors {
            border: BorderColor::default(),
            bg: navy.into(),
            text: muted.into(),
        };
        children.push(
            wrapped_text_element(&font, pair_label, max_cols, pair_colors)
                .item_type(UIItemType::HarborPairMobile)
                .line_height(Some(1.55))
                .padding(BoxDimension {
                    left: Dimension::Pixels(16.),
                    right: Dimension::Pixels(12.),
                    top: Dimension::Pixels(2.),
                    bottom: Dimension::Pixels(6.),
                })
                .min_width(Some(Dimension::Pixels(content_width)))
                .max_width(Some(Dimension::Pixels(content_width)))
                .hover_colors(Some(ElementColors {
                    border: BorderColor::default(),
                    bg: LinearRgba(0.035, 0.09, 0.12, 0.96).into(),
                    text: teal.into(),
                })),
        );

        if let Some(view) = harbor_mobile::pairing_view() {
            let info = format!(
                "QR opened in your image viewer.\n{}:{}\nexpires {}s · {} device(s)",
                view.host, view.port, view.expires_in_sec, view.device_count
            );
            children.push(
                wrapped_text_element(
                    &font,
                    &info,
                    max_cols,
                    ElementColors {
                        border: BorderColor::default(),
                        bg: navy.into(),
                        text: muted.into(),
                    },
                )
                .line_height(Some(1.2))
                .padding(BoxDimension {
                    left: Dimension::Pixels(12.),
                    right: Dimension::Pixels(8.),
                    top: Dimension::Pixels(4.),
                    bottom: Dimension::Pixels(4.),
                })
                .min_width(Some(Dimension::Pixels(content_width)))
                .max_width(Some(Dimension::Pixels(content_width))),
            );

            let sidebar_link = |label: &str, item: UIItemType| {
                Element::new(&font, ElementContent::Text(label.to_string()))
                    .display(DisplayType::Block)
                    .item_type(item)
                    .line_height(Some(1.45))
                    .padding(BoxDimension {
                        left: Dimension::Pixels(16.),
                        right: Dimension::Pixels(12.),
                        top: Dimension::Pixels(3.),
                        bottom: Dimension::Pixels(3.),
                    })
                    .min_width(Some(Dimension::Pixels(content_width)))
                    .max_width(Some(Dimension::Pixels(content_width)))
                    .colors(ElementColors {
                        border: BorderColor::default(),
                        bg: navy.into(),
                        text: teal.into(),
                    })
                    .hover_colors(Some(ElementColors {
                        border: BorderColor::default(),
                        bg: LinearRgba(0.035, 0.09, 0.12, 0.96).into(),
                        text: teal.into(),
                    }))
            };
            children.push(sidebar_link(
                "🖼  Open QR image",
                UIItemType::HarborOpenPairQr,
            ));
            children.push(sidebar_link(
                "⧉  Copy pair URI",
                UIItemType::HarborCopyPairUri,
            ));

            children.push(
                wrapped_text_element(
                    &font,
                    &format!("URI:\n{}", view.uri),
                    max_cols,
                    ElementColors {
                        border: BorderColor::default(),
                        bg: navy.into(),
                        text: muted.into(),
                    },
                )
                .line_height(Some(1.15))
                .padding(BoxDimension {
                    left: Dimension::Pixels(12.),
                    right: Dimension::Pixels(8.),
                    top: Dimension::Pixels(6.),
                    bottom: Dimension::Pixels(2.),
                })
                .min_width(Some(Dimension::Pixels(content_width)))
                .max_width(Some(Dimension::Pixels(content_width))),
            );

            children.push(
                Element::new(&font, ElementContent::Text("↻  New QR code".to_string()))
                    .display(DisplayType::Block)
                    .item_type(UIItemType::HarborRefreshPair)
                    .line_height(Some(1.45))
                    .padding(BoxDimension {
                        left: Dimension::Pixels(16.),
                        right: Dimension::Pixels(12.),
                        top: Dimension::Pixels(4.),
                        bottom: Dimension::Pixels(8.),
                    })
                    .min_width(Some(Dimension::Pixels(content_width)))
                    .max_width(Some(Dimension::Pixels(content_width)))
                    .colors(ElementColors {
                        border: BorderColor::default(),
                        bg: navy.into(),
                        text: teal.into(),
                    })
                    .hover_colors(Some(ElementColors {
                        border: BorderColor::default(),
                        bg: LinearRgba(0.035, 0.09, 0.12, 0.96).into(),
                        text: teal.into(),
                    })),
            );
        }

        for row in harbor_workspace::rows() {
            let (colors, hover) = Self::sidebar_colors(row.selected);
            // Line 1 is the live directory; the creation-time workspace name is
            // deliberately not shown, since it never follows `cd` or tab
            // switches.
            let title = wrapped_text_element(
                &font,
                &format!(
                    "{}  {}",
                    row.activity.glyph(),
                    truncate_line(&row.directory, max_cols.saturating_sub(3))
                ),
                max_cols,
                colors.clone(),
            );
            let mut kids = vec![title];
            if let Some(detail) = workspace_detail(
                row.agent.as_deref(),
                row.summary.as_deref(),
                max_cols.saturating_sub(2),
            ) {
                // `workspace_detail` already truncated each line to
                // `max_cols - 2` and added the 2-space indent, so wrapping at
                // `max_cols` here only splits on the newline between them.
                kids.push(wrapped_text_element(
                    &font,
                    &detail,
                    max_cols,
                    ElementColors {
                        border: BorderColor::default(),
                        bg: InheritableColor::Inherited,
                        text: muted.into(),
                    },
                ));
            }
            children.push(
                Element::new(&font, ElementContent::Children(kids))
                    .display(DisplayType::Block)
                    .item_type(UIItemType::HarborWorkspace(row.workspace.mux_workspace))
                    .line_height(Some(1.25))
                    .padding(BoxDimension {
                        left: Dimension::Pixels(16.),
                        right: Dimension::Pixels(12.),
                        top: Dimension::Pixels(7.),
                        bottom: Dimension::Pixels(7.),
                    })
                    .min_width(Some(Dimension::Pixels(content_width)))
                    .max_width(Some(Dimension::Pixels(content_width)))
                    .colors(colors)
                    .hover_colors(hover),
            );
        }

        let root = Element::new(&font, ElementContent::Children(children))
            .display(DisplayType::Block)
            .min_width(Some(Dimension::Pixels(width)))
            .max_width(Some(Dimension::Pixels(width)))
            .min_height(Some(Dimension::Pixels(height)))
            .colors(ElementColors {
                border: BorderColor::default(),
                bg: navy.into(),
                text: LinearRgba(0.84, 0.89, 0.91, 1.0).into(),
            });

        let mut computed = self.compute_element(
            &LayoutContext {
                height: DimensionContext {
                    dpi: self.dimensions.dpi as f32,
                    pixel_max: height,
                    pixel_cell: metrics.cell_size.height as f32,
                },
                width: DimensionContext {
                    dpi: self.dimensions.dpi as f32,
                    pixel_max: width,
                    pixel_cell: metrics.cell_size.width as f32,
                },
                bounds: euclid::rect(0., 0., width, height),
                metrics: &metrics,
                gl_state: self.render_state.as_ref().unwrap(),
                // Share the established UI text layer used by the fancy tab bar.
                zindex: 10,
            },
            &root,
        )?;
        computed.translate(euclid::vec2(
            border.left.get() as f32,
            border.top.get() as f32,
        ));
        Ok(computed)
    }

    pub fn paint_harbor_sidebar(&mut self) -> anyhow::Result<()> {
        if self.harbor_sidebar_width() == 0 {
            return Ok(());
        }
        harbor_workspace::ensure_current_workspace(self.mux_window_id);
        // Pairing QR expiry countdown needs a fresh layout each frame.
        if harbor_mobile::pairing_ui_visible() {
            self.harbor_sidebar.take();
        }
        if self.harbor_sidebar.is_none() {
            self.harbor_sidebar = Some(self.build_harbor_sidebar()?);
        }
        let computed = self.harbor_sidebar.as_ref().unwrap();
        self.ui_items.append(&mut computed.ui_items());
        let gl_state = self.render_state.as_ref().unwrap();
        self.render_element(computed, gl_state, None)
    }

    pub fn harbor_create_workspace(&mut self) -> anyhow::Result<()> {
        let pane = self
            .get_active_pane_or_overlay()
            .ok_or_else(|| anyhow::anyhow!("no active pane"))?;
        let root = new_workspace_directory(dirs_next::home_dir())?;
        let workspace = harbor_workspace::create_from_path(root.clone());
        let action = config::keyassignment::KeyAssignment::SwitchToWorkspace {
            name: Some(workspace.mux_workspace),
            spawn: Some(config::keyassignment::SpawnCommand {
                cwd: Some(root),
                ..Default::default()
            }),
        };
        self.invalidate_harbor_sidebar();
        self.perform_key_assignment(&pane, &action)?;
        Ok(())
    }

    pub fn harbor_activate_workspace(
        &mut self,
        workspace: harbor_workspace::HarborWorkspace,
    ) -> anyhow::Result<()> {
        if !Mux::get()
            .iter_windows_in_workspace(&workspace.mux_workspace)
            .is_empty()
        {
            crate::frontend::front_end().switch_workspace(&workspace.mux_workspace);
            self.invalidate_harbor_sidebar();
            return Ok(());
        }

        let pane = self
            .get_active_pane_or_overlay()
            .ok_or_else(|| anyhow::anyhow!("no active pane"))?;
        let cwd = harbor_workspace::resume_cwd(&workspace);
        let action = config::keyassignment::KeyAssignment::SwitchToWorkspace {
            name: Some(workspace.mux_workspace),
            spawn: Some(config::keyassignment::SpawnCommand {
                cwd,
                // The old workspace's final pane may disappear while this
                // asynchronous spawn is starting. The Harbor session domain
                // remains available independently of that pane.
                domain: config::keyassignment::SpawnTabDomain::DefaultDomain,
                ..Default::default()
            }),
        };
        self.invalidate_harbor_sidebar();
        self.perform_key_assignment(&pane, &action)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{
        new_workspace_directory, truncate_line, unicode_column_width, workspace_detail,
        wrap_sidebar_lines,
    };

    #[test]
    fn new_workspace_starts_in_the_home_directory() {
        let home = tempfile::tempdir().unwrap();

        assert_eq!(
            new_workspace_directory(Some(home.path().to_path_buf())).unwrap(),
            home.path()
        );
    }

    #[test]
    fn new_workspace_requires_an_available_home_directory() {
        assert!(new_workspace_directory(None).is_err());
        let home = tempfile::tempdir().unwrap();
        let missing = home.path().join("missing");
        assert!(new_workspace_directory(Some(missing)).is_err());
    }

    #[test]
    fn wraps_long_uri_without_spaces() {
        let uri = "harbor://pair?v=1&host=192.168.1.20&port=7780&tls=0&token=abcdef";
        let wrapped = wrap_sidebar_lines(uri, 20);
        assert!(wrapped.len() > 1);
        for line in &wrapped {
            assert!(line.chars().count() <= 20);
        }
    }

    #[test]
    fn preserves_explicit_newlines() {
        let text = "line one\nline two";
        assert_eq!(wrap_sidebar_lines(text, 80), vec!["line one", "line two"]);
    }

    #[test]
    fn detail_is_absent_without_an_agent() {
        assert_eq!(workspace_detail(None, None, 40), None);
        // A summary without an agent must not produce an orphan line.
        assert_eq!(workspace_detail(None, Some("Running tests"), 40), None);
        assert_eq!(
            workspace_detail(Some("   "), Some("Running tests"), 40),
            None
        );
    }

    #[test]
    fn detail_lists_agent_then_summary() {
        assert_eq!(
            workspace_detail(Some("Codex"), None, 40),
            Some("  Codex".to_string())
        );
        assert_eq!(
            workspace_detail(Some("Codex"), Some("Running tests"), 40),
            Some("  Codex\n  Running tests".to_string())
        );
        assert_eq!(
            workspace_detail(Some("Codex"), Some("  "), 40),
            Some("  Codex".to_string())
        );
    }

    #[test]
    fn detail_lines_fit_the_row_width_including_the_indent() {
        // Rows must stay one visual line: each detail line is truncated to
        // `max_cols - 2` and then indented by two spaces, so the rendered line
        // has to come out at or under `max_cols`.
        let max_cols = 40;
        let long_ascii = "a".repeat(200);
        let long_cjk = "作業内容の要約がとても長い場合のテキスト".repeat(4);
        for summary in [long_ascii.as_str(), long_cjk.as_str()] {
            let detail = workspace_detail(Some("Claude"), Some(summary), max_cols - 2).unwrap();
            let lines: Vec<_> = detail.split('\n').collect();
            assert_eq!(lines.len(), 2);
            for line in lines {
                assert!(
                    unicode_column_width(line, None) <= max_cols,
                    "line {line:?} exceeds {max_cols} cells"
                );
            }
        }
    }

    #[test]
    fn truncate_line_keeps_short_text_intact() {
        assert_eq!(truncate_line("harbor", 40), "harbor");
        assert_eq!(truncate_line("  harbor  ", 40), "harbor");
    }

    #[test]
    fn truncate_line_collapses_newlines_into_one_row() {
        assert_eq!(truncate_line("line one\nline two", 40), "line one line two");
    }

    #[test]
    fn truncate_line_marks_ascii_overflow() {
        let out = truncate_line("abcdefghij", 5);
        assert_eq!(out, "abcd…");
        assert!(unicode_column_width(&out, None) <= 5);
    }

    #[test]
    fn truncate_line_respects_double_width_cells() {
        // Each CJK character occupies two cells, so 10 cells fit 4 of them
        // plus the ellipsis.
        let out = truncate_line("作業内容の要約が長い", 10);
        assert_eq!(out, "作業内容…");
        assert!(unicode_column_width(&out, None) <= 10);
    }
}
