use crate::harbor_workspace;
use crate::termwindow::box_model::*;
use crate::termwindow::UIItemType;
use crate::utilsprites::RenderMetrics;
use config::{Dimension, DimensionContext};
use mux::pane::CachePolicy;
use mux::Mux;
use std::path::PathBuf;
use window::color::LinearRgba;

impl crate::TermWindow {
    pub fn harbor_sidebar_width(&self) -> usize {
        if !harbor_workspace::sidebar_visible() || self.dimensions.pixel_width < 560 {
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
        let font = self.fonts.title_font()?;
        let metrics = RenderMetrics::with_font_metrics(&font.metrics());
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
            let title = Element::new(
                &font,
                ElementContent::Text(format!("{}  {}", row.activity.glyph(), row.workspace.name)),
            )
            .display(DisplayType::Block)
            .colors(colors.clone());
            let detail = Element::new(&font, ElementContent::Text(detail))
                .display(DisplayType::Block)
                .colors(ElementColors {
                    border: BorderColor::default(),
                    bg: InheritableColor::Inherited,
                    text: muted.into(),
                });
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
