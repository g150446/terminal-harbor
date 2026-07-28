use crate::harbor_mobile;
use crate::harbor_workspace;
use crate::termwindow::box_model::*;
use crate::termwindow::UIItemType;
use crate::utilsprites::RenderMetrics;
use config::{Dimension, DimensionContext};
use mux::pane::CachePolicy;
use mux::Mux;
use std::path::PathBuf;
use window::color::LinearRgba;

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

/// box_model Text is single-line; multi-line content must be Children of Text rows.
fn wrapped_text_element(
    font: &std::rc::Rc<wezterm_font::LoadedFont>,
    text: &str,
    max_cols: usize,
    colors: ElementColors,
) -> Element {
    let lines = wrap_sidebar_lines(text, max_cols);
    if lines.len() == 1 {
        return Element::new(font, ElementContent::Text(lines.into_iter().next().unwrap()))
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
            children.push(sidebar_link("🖼  Open QR image", UIItemType::HarborOpenPairQr));
            children.push(sidebar_link("⧉  Copy pair URI", UIItemType::HarborCopyPairUri));

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
            let location = row
                .workspace
                .root
                .as_ref()
                .map(|root| root.display().to_string())
                .unwrap_or_else(|| "Session workspace".to_string());
            let detail = match (row.process.as_deref(), row.message.as_deref()) {
                (_, Some(message)) if !message.is_empty() => format!("  {message}"),
                (Some(process), _) => format!("  {location} · {process}"),
                _ => format!("  {location}"),
            };
            let (colors, hover) = Self::sidebar_colors(row.selected);
            let title = wrapped_text_element(
                &font,
                &format!("{}  {}", row.activity.glyph(), row.workspace.name),
                max_cols,
                colors.clone(),
            );
            let detail = wrapped_text_element(
                &font,
                &detail,
                max_cols.saturating_sub(2),
                ElementColors {
                    border: BorderColor::default(),
                    bg: InheritableColor::Inherited,
                    text: muted.into(),
                },
            );
            children.push(
                Element::new(&font, ElementContent::Children(vec![title, detail]))
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
        let root = pane
            .get_current_working_dir(CachePolicy::AllowStale)
            .and_then(|url| url.to_file_path().ok())
            .or_else(dirs_next::home_dir)
            .unwrap_or_else(|| PathBuf::from("."));
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
        let action = config::keyassignment::KeyAssignment::SwitchToWorkspace {
            name: Some(workspace.mux_workspace),
            spawn: Some(config::keyassignment::SpawnCommand {
                cwd: workspace.root,
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
    use super::wrap_sidebar_lines;

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
}
