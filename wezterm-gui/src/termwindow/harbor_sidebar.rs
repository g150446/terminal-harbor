use crate::termwindow::UIItemType;
use crate::termwindow::box_model::*;
use crate::utilsprites::RenderMetrics;
use crate::{harbor_mobile, harbor_peer, harbor_workspace};
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

/// First line of a local workspace row: 1-based index, activity glyph, directory.
/// The index is display order, matching `ActivateWorkspace` / `⌘1`–`⌘9`.
/// Remote peer rows do not use this prefix.
fn workspace_title_line(index: usize, glyph: &str, directory: &str, max_cols: usize) -> String {
    let prefix = format!("{}  {}", index + 1, glyph);
    let reserved = unicode_column_width(&prefix, None) + 2;
    format!(
        "{}  {}",
        prefix,
        truncate_line(directory, max_cols.saturating_sub(reserved))
    )
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

/// Choose which body rows to lay out, as `start..end`.
///
/// When every row fits in `available` the whole list is shown. Otherwise room
/// is reserved for the "more" indicators above and below, `scroll` is clamped
/// so the last row can always be reached, and `follow` (a row index) is
/// brought into view with the smallest change of offset.
fn sidebar_visible_range(
    heights: &[f32],
    available: f32,
    indicator: f32,
    scroll: usize,
    follow: Option<usize>,
) -> (usize, usize) {
    let len = heights.len();
    if len == 0 || heights.iter().sum::<f32>() <= available {
        return (0, len);
    }
    let room = (available - 2. * indicator).max(0.);
    // Smallest start whose rows from `start` through `last` all fit, keeping
    // at least `last` itself.
    let start_ending_at = |last: usize| {
        let mut used = 0.;
        let mut start = last + 1;
        while start > 0 && used + heights[start - 1] <= room {
            used += heights[start - 1];
            start -= 1;
        }
        start.min(last)
    };
    let max_start = start_ending_at(len - 1);
    let mut start = scroll.min(max_start);
    let end_from = |start: usize| {
        let mut used = 0.;
        let mut end = start;
        while end < len && used + heights[end] <= room {
            used += heights[end];
            end += 1;
        }
        end.max(start + 1)
    };
    if let Some(row) = follow.filter(|row| *row < len) {
        if row < start {
            start = row;
        } else if row >= end_from(start) {
            start = start_ending_at(row);
        }
    }
    (start, end_from(start))
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

    fn build_harbor_sidebar(&mut self) -> anyhow::Result<ComputedElement> {
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
        // The header stays pinned; only `children` (the body rows) scroll.
        let mut header = vec![];
        let mut children = vec![];
        let mut selected_row: Option<(usize, String)> = None;

        header.push(
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

        header.push(
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
            wrapped_text_element(&font, pair_label, max_cols, pair_colors.clone())
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

        children.push(
            wrapped_text_element(
                &font,
                "🖥  Pair another Harbor",
                max_cols,
                pair_colors.clone(),
            )
            .item_type(UIItemType::HarborPairPeer)
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
        children.push(
            wrapped_text_element(
                &font,
                "⇄  Pair via Tailscale (clipboard)",
                max_cols,
                pair_colors.clone(),
            )
            .item_type(UIItemType::HarborPairPeerTailscale)
            .line_height(Some(1.55))
            .padding(BoxDimension {
                left: Dimension::Pixels(16.),
                right: Dimension::Pixels(12.),
                top: Dimension::Pixels(2.),
                bottom: Dimension::Pixels(2.),
            })
            .min_width(Some(Dimension::Pixels(content_width)))
            .max_width(Some(Dimension::Pixels(content_width)))
            .hover_colors(Some(ElementColors {
                border: BorderColor::default(),
                bg: LinearRgba(0.035, 0.09, 0.12, 0.96).into(),
                text: teal.into(),
            })),
        );

        // The switch governs this machine as a host, so it reads as the state
        // peers see when they reach the bridge.
        let tailnet_label = if harbor_mobile::tailnet_pairing_enabled() {
            "🔓  Tailscale auto-pair: on"
        } else {
            "🔒  Tailscale auto-pair: off"
        };
        children.push(
            wrapped_text_element(&font, tailnet_label, max_cols, pair_colors.clone())
                .item_type(UIItemType::HarborToggleTailnetPair)
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

        if let Some(status) = harbor_peer::pair_status() {
            children.push(
                wrapped_text_element(
                    &font,
                    &status,
                    max_cols,
                    ElementColors {
                        border: BorderColor::default(),
                        bg: navy.into(),
                        text: muted.into(),
                    },
                )
                .line_height(Some(1.2))
                .padding(BoxDimension {
                    left: Dimension::Pixels(16.),
                    right: Dimension::Pixels(12.),
                    top: Dimension::Pixels(2.),
                    bottom: Dimension::Pixels(6.),
                })
                .min_width(Some(Dimension::Pixels(content_width)))
                .max_width(Some(Dimension::Pixels(content_width))),
            );
        }

        for peer in harbor_peer::snapshot() {
            children.push(
                wrapped_text_element(
                    &font,
                    &format!(
                        "▸  {}",
                        truncate_line(&peer.label, max_cols.saturating_sub(3))
                    ),
                    max_cols,
                    ElementColors {
                        border: BorderColor::default(),
                        bg: navy.into(),
                        text: teal.into(),
                    },
                )
                .line_height(Some(1.35))
                .padding(BoxDimension {
                    left: Dimension::Pixels(16.),
                    right: Dimension::Pixels(12.),
                    top: Dimension::Pixels(8.),
                    bottom: Dimension::Pixels(2.),
                })
                .min_width(Some(Dimension::Pixels(content_width)))
                .max_width(Some(Dimension::Pixels(content_width))),
            );
            if let Some(error) = peer.error.as_deref().filter(|value| !value.is_empty()) {
                children.push(
                    wrapped_text_element(
                        &font,
                        error,
                        max_cols,
                        ElementColors {
                            border: BorderColor::default(),
                            bg: navy.into(),
                            text: muted.into(),
                        },
                    )
                    .line_height(Some(1.2))
                    .padding(BoxDimension {
                        left: Dimension::Pixels(16.),
                        right: Dimension::Pixels(12.),
                        top: Dimension::Pixels(2.),
                        bottom: Dimension::Pixels(2.),
                    })
                    .min_width(Some(Dimension::Pixels(content_width)))
                    .max_width(Some(Dimension::Pixels(content_width))),
                );
            }
            if peer.needs_lan_confirm {
                children.push(
                    wrapped_text_element(
                        &font,
                        "Use local network",
                        max_cols,
                        ElementColors {
                            border: BorderColor::default(),
                            bg: navy.into(),
                            text: teal.into(),
                        },
                    )
                    .item_type(UIItemType::HarborConfirmLan(peer.server_id.clone()))
                    .line_height(Some(1.35))
                    .padding(BoxDimension {
                        left: Dimension::Pixels(16.),
                        right: Dimension::Pixels(12.),
                        top: Dimension::Pixels(2.),
                        bottom: Dimension::Pixels(4.),
                    })
                    .min_width(Some(Dimension::Pixels(content_width)))
                    .max_width(Some(Dimension::Pixels(content_width)))
                    .hover_colors(Some(ElementColors {
                        border: BorderColor::default(),
                        bg: LinearRgba(0.035, 0.09, 0.12, 0.96).into(),
                        text: teal.into(),
                    })),
                );
            }
            for remote in peer.workspaces {
                let (colors, hover) = Self::sidebar_colors(remote.selected);
                children.push(
                    wrapped_text_element(
                        &font,
                        &format!(
                            "{}  {}",
                            remote.activity.glyph(),
                            truncate_line(&remote.directory, max_cols.saturating_sub(3))
                        ),
                        max_cols,
                        colors,
                    )
                    .item_type(UIItemType::HarborRemoteWorkspace {
                        server_id: peer.server_id.clone(),
                        workspace_id: remote.id,
                    })
                    .line_height(Some(1.25))
                    .padding(BoxDimension {
                        left: Dimension::Pixels(24.),
                        right: Dimension::Pixels(12.),
                        top: Dimension::Pixels(5.),
                        bottom: Dimension::Pixels(5.),
                    })
                    .min_width(Some(Dimension::Pixels(content_width)))
                    .max_width(Some(Dimension::Pixels(content_width)))
                    .hover_colors(hover),
                );
            }
            children.push(
                wrapped_text_element(
                    &font,
                    "Unpair",
                    max_cols,
                    ElementColors {
                        border: BorderColor::default(),
                        bg: navy.into(),
                        text: muted.into(),
                    },
                )
                .item_type(UIItemType::HarborUnpairPeer(peer.server_id.clone()))
                .line_height(Some(1.3))
                .padding(BoxDimension {
                    left: Dimension::Pixels(16.),
                    right: Dimension::Pixels(12.),
                    top: Dimension::Pixels(2.),
                    bottom: Dimension::Pixels(8.),
                })
                .min_width(Some(Dimension::Pixels(content_width)))
                .max_width(Some(Dimension::Pixels(content_width)))
                .hover_colors(Some(ElementColors {
                    border: BorderColor::default(),
                    bg: LinearRgba(0.035, 0.09, 0.12, 0.96).into(),
                    text: teal.into(),
                })),
            );
        }

        for (index, row) in harbor_workspace::rows().into_iter().enumerate() {
            let (colors, hover) = Self::sidebar_colors(row.selected);
            if row.selected {
                selected_row = Some((children.len(), row.workspace.mux_workspace.clone()));
            }
            // Line 1 is the live directory; the creation-time workspace name is
            // deliberately not shown, since it never follows `cd` or tab
            // switches.
            let title = wrapped_text_element(
                &font,
                &workspace_title_line(index, row.activity.glyph(), &row.directory, max_cols),
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

        let indicator = |text: String| {
            Element::new(&font, ElementContent::Text(text))
                .display(DisplayType::Block)
                .line_height(Some(1.3))
                .padding(BoxDimension {
                    left: Dimension::Pixels(16.),
                    right: Dimension::Pixels(12.),
                    top: Dimension::Pixels(2.),
                    bottom: Dimension::Pixels(2.),
                })
                .min_width(Some(Dimension::Pixels(content_width)))
                .max_width(Some(Dimension::Pixels(content_width)))
                .colors(ElementColors {
                    border: BorderColor::default(),
                    bg: navy.into(),
                    text: muted.into(),
                })
        };
        let root_colors = ElementColors {
            border: BorderColor::default(),
            bg: navy.into(),
            text: LinearRgba(0.84, 0.89, 0.91, 1.0).into(),
        };
        let context = LayoutContext {
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
        };

        // Measure every row once, then lay out only the rows that fit. Whole
        // rows are skipped rather than clipped, because the box model has no
        // clip rectangle and a partially hidden row would paint over the
        // window's top edge.
        let mut measure = header.clone();
        measure.extend(children.iter().cloned());
        measure.push(indicator("▾  0 more".to_string()));
        let measured = self.compute_element(
            &context,
            &Element::new(&font, ElementContent::Children(measure))
                .display(DisplayType::Block)
                .min_width(Some(Dimension::Pixels(width)))
                .max_width(Some(Dimension::Pixels(width))),
        )?;
        let heights: Vec<f32> = match &measured.content {
            ComputedElementContent::Children(kids) => {
                kids.iter().map(|kid| kid.bounds.height()).collect()
            }
            _ => vec![],
        };
        let header_height: f32 = heights.iter().take(header.len()).sum();
        let indicator_height = heights.last().copied().unwrap_or(0.);
        let row_heights = heights
            .get(header.len()..heights.len().saturating_sub(1))
            .unwrap_or(&[]);

        // Follow the selection only when it changes, so a rebuild for
        // activity updates does not undo the user's own scrolling.
        let follow = match &selected_row {
            Some((index, name)) if self.harbor_sidebar_followed.as_ref() != Some(name) => {
                Some(*index)
            }
            _ => None,
        };
        self.harbor_sidebar_followed = selected_row.map(|(_, name)| name);
        let (start, end) = sidebar_visible_range(
            row_heights,
            height - header_height,
            indicator_height,
            self.harbor_sidebar_scroll,
            follow,
        );
        self.harbor_sidebar_scroll = start;

        let total = children.len();
        let mut visible = header;
        if start > 0 {
            visible.push(indicator(format!("▴  {start} more")));
        }
        visible.extend(children.into_iter().skip(start).take(end - start));
        if end < total {
            visible.push(indicator(format!("▾  {} more", total - end)));
        }

        let root = Element::new(&font, ElementContent::Children(visible))
            .display(DisplayType::Block)
            .min_width(Some(Dimension::Pixels(width)))
            .max_width(Some(Dimension::Pixels(width)))
            .min_height(Some(Dimension::Pixels(height)))
            .colors(root_colors);

        let mut computed = self.compute_element(&context, &root)?;
        computed.translate(euclid::vec2(
            border.left.get() as f32,
            border.top.get() as f32,
        ));
        Ok(computed)
    }

    /// Scroll the sidebar body by whole rows; positive `delta` moves toward
    /// the top. The next build clamps the offset to the rows that exist.
    pub fn scroll_harbor_sidebar(&mut self, delta: isize) {
        self.harbor_sidebar_scroll = if delta > 0 {
            self.harbor_sidebar_scroll.saturating_sub(delta as usize)
        } else {
            self.harbor_sidebar_scroll
                .saturating_add(delta.unsigned_abs())
        };
        self.invalidate_harbor_sidebar();
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

    pub fn harbor_open_remote_workspace(&mut self, server_id: String, workspace_id: String) {
        let mux = Mux::get();
        let tab = match mux.get_active_tab_for_window(self.mux_window_id) {
            Some(tab) => tab,
            None => return,
        };
        let host_label = crate::harbor_peer::peer_label(&server_id);
        let (overlay, future) = crate::overlay::start_overlay(self, &tab, move |_tab_id, term| {
            crate::overlay::harbor_remote::show(term, server_id, workspace_id, host_label)
        });
        self.assign_overlay(tab.tab_id(), overlay);
        promise::spawn::spawn(future).detach();
    }

    /// Pair using the clipboard content. The sidebar has no dialog to show, so
    /// a failure lands in the log and in the peer status line.
    fn harbor_pair_with_clipboard(
        context: &dyn window::WindowOps,
        pair: fn(&str) -> anyhow::Result<()>,
        what: &'static str,
    ) {
        let future = context.get_clipboard(window::Clipboard::Clipboard);
        promise::spawn::spawn(async move {
            match future.await {
                Ok(text) => {
                    promise::spawn::spawn_into_new_thread(move || {
                        if let Err(err) = pair(&text) {
                            log::error!("pairing another Harbor: {err:#}");
                        }
                        Ok::<(), anyhow::Error>(())
                    })
                    .detach();
                }
                Err(err) => log::error!("reading {what} from clipboard: {err:#}"),
            }
        })
        .detach();
    }

    pub fn harbor_pair_peer_from_clipboard(&mut self, context: &dyn window::WindowOps) {
        Self::harbor_pair_with_clipboard(context, crate::harbor_peer::pair_from_uri, "pair URI");
    }

    /// The no-code path: the clipboard holds a Tailscale device id instead of a
    /// pair URI. A URI is still accepted, so both rows work with either input.
    pub fn harbor_pair_peer_tailscale_from_clipboard(&mut self, context: &dyn window::WindowOps) {
        Self::harbor_pair_with_clipboard(
            context,
            crate::harbor_peer::pair_from_text,
            "Tailscale device id",
        );
    }
}

#[cfg(test)]
mod tests {
    use super::{
        new_workspace_directory, sidebar_visible_range, truncate_line, unicode_column_width,
        workspace_detail, workspace_title_line, wrap_sidebar_lines,
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
    fn title_line_numbers_from_one_in_display_order() {
        assert_eq!(workspace_title_line(0, "●", "harbor", 40), "1  ●  harbor");
        assert_eq!(workspace_title_line(9, "○", "harbor", 40), "10  ○  harbor");
    }

    #[test]
    fn title_line_fits_the_row_width_including_the_index() {
        let max_cols = 20;
        let long_ascii = "a".repeat(200);
        let long_cjk = "作業内容の要約がとても長い場合のテキスト".repeat(4);
        for (index, directory) in [(0usize, long_ascii.as_str()), (9, long_cjk.as_str())] {
            let line = workspace_title_line(index, "●", directory, max_cols);
            assert!(
                unicode_column_width(&line, None) <= max_cols,
                "line {:?} exceeds {} cells",
                line,
                max_cols
            );
        }
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
                    "line {:?} exceeds {} cells",
                    line,
                    max_cols
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

    #[test]
    fn visible_range_shows_everything_when_rows_fit() {
        assert_eq!(
            sidebar_visible_range(&[10., 10., 10.], 30., 5., 2, None),
            (0, 3)
        );
        assert_eq!(sidebar_visible_range(&[], -4., 5., 3, None), (0, 0));
    }

    #[test]
    fn visible_range_reserves_room_for_both_indicators() {
        // 100px minus two 10px indicators leaves room for four 20px rows.
        assert_eq!(
            sidebar_visible_range(&[20.; 10], 100., 10., 0, None),
            (0, 4)
        );
        assert_eq!(
            sidebar_visible_range(&[20.; 10], 100., 10., 3, None),
            (3, 7)
        );
    }

    #[test]
    fn visible_range_clamps_scroll_so_the_last_row_stays_reachable() {
        assert_eq!(
            sidebar_visible_range(&[20.; 10], 100., 10., 99, None),
            (6, 10)
        );
    }

    #[test]
    fn visible_range_handles_rows_of_different_heights() {
        let heights = [10., 40., 10., 10., 30., 10.];
        // Room is 60: from the end, 10 + 30 + 10 + 10 fit, 40 does not.
        assert_eq!(sidebar_visible_range(&heights, 80., 10., 99, None), (2, 6));
        assert_eq!(sidebar_visible_range(&heights, 80., 10., 0, None), (0, 3));
    }

    #[test]
    fn visible_range_always_shows_at_least_one_row() {
        assert_eq!(
            sidebar_visible_range(&[50., 50.], 40., 10., 0, None),
            (0, 1)
        );
        assert_eq!(
            sidebar_visible_range(&[50., 50.], 40., 10., 9, None),
            (1, 2)
        );
    }

    #[test]
    fn visible_range_scrolls_the_followed_row_into_view() {
        // Below the window: the row becomes the last one shown.
        assert_eq!(
            sidebar_visible_range(&[20.; 10], 100., 10., 0, Some(8)),
            (5, 9)
        );
        // Above the window: the row becomes the first one shown.
        assert_eq!(
            sidebar_visible_range(&[20.; 10], 100., 10., 6, Some(2)),
            (2, 6)
        );
        // Already visible: the offset is left alone.
        assert_eq!(
            sidebar_visible_range(&[20.; 10], 100., 10., 3, Some(5)),
            (3, 7)
        );
    }
}
