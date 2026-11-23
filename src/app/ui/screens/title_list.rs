use crate::api::catalog::Game;
use crate::app::ui::header::show_header_row;
use crate::app::ui::theme::Theme;
use crate::app::ui::widgets::{draw_title_image, draw_title_image_cover};
use crate::app::{AppState, StreamStartTarget, TitleImage};
use crate::i18n::I18n;
use crate::{App, AppCommand, InputCommand, StreamKind};
use anyhow::Result;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Covers per grid row.
pub(crate) const GRID_COLUMNS: usize = 6;
const GRID_GAP: f32 = 8.0;
/// Opaque strip behind the header: 8pt panel margin + 36pt logo + 8pt `add_space` below the
/// row. Must stay in sync with `show_header_row` if either changes.
const HEADER_TINT_HEIGHT: f32 = 8.0 + 36.0 + 8.0;

#[derive(Clone, PartialEq)]
pub enum Command {
    OpenSearch,
    SetSearch(String),
    /// Highlight this title (index into `service.titles`).
    Select(usize),
    OpenDetails,
    CloseDetails,
    SelectCategory(usize),
    /// Grid positions on screen: first and one-past-last.
    SetVisible(usize, usize),
}

pub(crate) enum CategoryLabel {
    /// Translation id.
    Text(&'static str),
    /// Title given by the server, already translated.
    Server(String),
}

/// One tab of the game grid.
pub(crate) struct Category {
    pub(crate) key: &'static str,
    pub(crate) label: CategoryLabel,
    /// Indices into `service.titles`, in display order.
    pub(crate) indices: Vec<usize>,
}

/// Tabs in order: recently played (if any), the Game Pass lists that loaded, all games.
pub(crate) fn categories(app: &App) -> Vec<Category> {
    let mut categories = Vec::new();
    let recent = app.recent_title_indices();
    if !recent.is_empty() {
        categories.push(Category {
            key: "recent",
            label: CategoryLabel::Text("title-category-recent"),
            indices: recent,
        });
    }
    for collection in &app.service.collections {
        categories.push(Category {
            key: collection.key,
            label: CategoryLabel::Server(collection.title.clone()),
            indices: collection.indices.clone(),
        });
    }
    categories.push(Category {
        key: "all",
        label: CategoryLabel::Text("title-category-all"),
        indices: (0..app.service.titles.len()).collect(),
    });
    categories
}

fn searching(app: &App) -> bool {
    !app.title_search_query.trim().is_empty()
}

/// Titles shown in the grid, in order: the search results when a search is active, otherwise
/// the current category.
pub(crate) fn grid_title_indices(app: &App) -> Vec<usize> {
    let query = app.title_search_query.trim();
    if !query.is_empty() {
        return app
            .service
            .titles
            .iter()
            .enumerate()
            .filter_map(|(index, title)| {
                (contains_case_insensitive(title.display_name(), query)
                    || contains_case_insensitive(&title.id, query))
                .then_some(index)
            })
            .collect();
    }
    let mut categories = categories(app);
    if categories.is_empty() {
        return Vec::new();
    }
    let category = app.title_category.min(categories.len() - 1);
    categories.swap_remove(category).indices
}

fn contains_case_insensitive(text: &str, query: &str) -> bool {
    if query.is_empty() {
        return true;
    }
    if query.is_ascii() {
        return text
            .as_bytes()
            .windows(query.len())
            .any(|window| window.eq_ignore_ascii_case(query.as_bytes()));
    }
    text.to_lowercase().contains(&query.to_lowercase())
}

/// Cloud games screen: category tabs + search on top, a grid of covers below. Cross opens
/// the details page of the highlighted game, with its Play button.
pub(crate) fn show(ctx: &egui::Context, app: &App, commands: &mut Vec<AppCommand>) {
    let selected = match &app.state {
        AppState::TitleList { selected } => Some(*selected),
        AppState::LoadingTitles(_) => None,
        _ => return,
    };
    let theme = Theme::dark();
    let i18n = I18n::new(app.settings.locale);
    let mut frame = egui::Frame::central_panel(&ctx.style());
    frame.fill = egui::Color32::TRANSPARENT;
    egui::CentralPanel::default().frame(frame).show(ctx, |ui| {
        if app.title_details_open
            && let Some(title) = app.highlighted_title()
        {
            draw_title_background(ui, app, theme);
            draw_header_strip(ui, theme);
            show_header_row(ui, app, theme, &i18n, Some(commands));
            ui.add_space(8.0);
            show_details(ui, app, title, theme, &i18n, commands);
            return;
        }

        ui.painter()
            .rect_filled(ui.ctx().screen_rect(), 0.0, theme.background);
        show_header_row(ui, app, theme, &i18n, Some(commands));
        ui.add_space(8.0);
        show_tab_bar(ui, app, theme, &i18n, commands);
        ui.add_space(6.0);

        if app.service.titles.is_empty() {
            ui.vertical_centered(|ui| {
                ui.add_space(40.0);
                if matches!(&app.state, AppState::LoadingTitles(_)) {
                    ui.spinner();
                    ui.colored_label(theme.text, i18n.text("title-requesting"));
                } else {
                    ui.colored_label(theme.text_bright, i18n.text("title-none-returned"));
                    ui.colored_label(theme.text, i18n.text("title-token-hint"));
                }
            });
            return;
        }
        let grid = grid_title_indices(app);
        if grid.is_empty() {
            ui.vertical_centered(|ui| {
                ui.add_space(40.0);
                ui.colored_label(theme.text, i18n.text("title-search-empty"));
            });
            return;
        }
        let selected_position =
            selected.and_then(|selected| grid.iter().position(|i| *i == selected));
        show_grid(ui, app, &grid, selected_position, theme, commands);
    });
}

fn draw_header_strip(ui: &mut egui::Ui, theme: Theme) {
    let screen_rect = ui.ctx().screen_rect();
    let bg = theme.background;
    let rect = egui::Rect::from_min_size(
        screen_rect.min,
        egui::vec2(screen_rect.width(), HEADER_TINT_HEIGHT),
    );
    ui.painter().rect_filled(
        rect,
        0.0,
        egui::Color32::from_rgba_unmultiplied(bg.r(), bg.g(), bg.b(), 235),
    );
}

/// Category tabs (L/R) on the left, search (Triangle) on the right.
fn show_tab_bar(
    ui: &mut egui::Ui,
    app: &App,
    theme: Theme,
    i18n: &I18n,
    commands: &mut Vec<AppCommand>,
) {
    const HEIGHT: f32 = 30.0;
    const SEARCH_WIDTH: f32 = 170.0;
    const CLEAR_WIDTH: f32 = 30.0;
    let tab_font = egui::FontId::proportional(14.0);
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        let tabs_width = (ui.available_width() - SEARCH_WIDTH - CLEAR_WIDTH - 12.0).max(0.0);
        ui.allocate_ui_with_layout(
            egui::vec2(tabs_width, HEIGHT),
            egui::Layout::left_to_right(egui::Align::Center),
            |ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                if searching(app) {
                    let count = grid_title_indices(app).len();
                    ui.colored_label(
                        theme.text_bright,
                        egui::RichText::new(format!(
                            "{} ({count})",
                            i18n.text("title-category-results")
                        ))
                        .font(tab_font.clone()),
                    );
                    return;
                }
                let categories = categories(app);
                let current = app.title_category.min(categories.len().saturating_sub(1));
                // Fixed slots: "L" | tabs (scroll sideways, clipped) | "R". Nothing can spill
                // over the search button.
                const SIDE_WIDTH: f32 = 14.0;
                let show_shoulders = categories.len() > 1;
                let mut shoulder = |ui: &mut egui::Ui, text: &str| {
                    let (rect, _) = ui
                        .allocate_exact_size(egui::vec2(SIDE_WIDTH, HEIGHT), egui::Sense::hover());
                    ui.painter().text(
                        rect.center(),
                        egui::Align2::CENTER_CENTER,
                        text,
                        tab_font.clone(),
                        theme.text,
                    );
                };
                if show_shoulders {
                    shoulder(ui, "L");
                }
                let reserved = if show_shoulders {
                    SIDE_WIDTH + ui.spacing().item_spacing.x
                } else {
                    0.0
                };
                let scroll_width = (ui.available_width() - reserved).max(0.0);
                let (scroll_rect, _) =
                    ui.allocate_exact_size(egui::vec2(scroll_width, HEIGHT), egui::Sense::hover());
                let mut tabs_ui = ui.new_child(
                    egui::UiBuilder::new()
                        .max_rect(scroll_rect)
                        .layout(egui::Layout::left_to_right(egui::Align::Center)),
                );
                tabs_ui.set_clip_rect(scroll_rect);
                egui::ScrollArea::horizontal()
                    .id_salt("title_tabs")
                    .max_width(scroll_width)
                    .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
                    .auto_shrink([false, true])
                    .show(&mut tabs_ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 6.0;
                            for (index, category) in categories.iter().enumerate() {
                                let active = index == current;
                                let name = match &category.label {
                                    CategoryLabel::Text(id) => i18n.text(id),
                                    CategoryLabel::Server(title) => title.clone(),
                                };
                                let label = format!("{name} ({})", category.indices.len());
                                let button = egui::Button::new(
                                    egui::RichText::new(label).font(tab_font.clone()).color(
                                        if active {
                                            egui::Color32::WHITE
                                        } else {
                                            theme.text
                                        },
                                    ),
                                )
                                .fill(if active {
                                    theme.accent
                                } else {
                                    egui::Color32::from_rgb(0x26, 0x27, 0x2c)
                                })
                                .corner_radius(14.0)
                                .min_size(egui::vec2(0.0, HEIGHT - 4.0));
                                let response = ui.add(button);
                                // Only when the tab changes, so a finger drag is not undone.
                                let last_id = egui::Id::new("title_tabs_last_active");
                                let last: Option<usize> = ui.data(|d| d.get_temp(last_id));
                                if active && last != Some(index) {
                                    response.scroll_to_me(Some(egui::Align::Center));
                                    ui.data_mut(|d| d.insert_temp(last_id, index));
                                }
                                if response.clicked() {
                                    commands.push(Command::SelectCategory(index).into());
                                }
                            }
                            if app.collections_loading() {
                                ui.spinner();
                            }
                        });
                    });
                if show_shoulders {
                    shoulder(ui, "R");
                }
            },
        );

        let label = if app.title_search_query.is_empty() {
            format!("\u{25b3} {}", i18n.text("title-search"))
        } else {
            format!("\u{25b3} {}", app.title_search_query)
        };
        let search = egui::Button::new(egui::RichText::new(label).font(tab_font.clone()).color(
            if app.title_search_query.is_empty() {
                theme.text
            } else {
                theme.text_bright
            },
        ))
        .truncate();
        if ui
            .add_sized(egui::vec2(SEARCH_WIDTH, HEIGHT), search)
            .clicked()
        {
            commands.push(Command::OpenSearch.into());
        }
        let clear = ui.add_enabled(
            !app.title_search_query.is_empty(),
            egui::Button::new(egui::RichText::new("\u{d7}").size(18.0))
                .min_size(egui::vec2(CLEAR_WIDTH, HEIGHT)),
        );
        if clear.clicked() {
            commands.push(Command::SetSearch(String::new()).into());
        }
    });
}

/// Grid of covers. Only the rows on screen are drawn; scrolling follows the selection (D-pad)
/// or the finger (drag), and the UI reports which positions are visible so covers off screen
/// can be freed.
fn show_grid(
    ui: &mut egui::Ui,
    app: &App,
    grid: &[usize],
    selected_position: Option<usize>,
    theme: Theme,
    commands: &mut Vec<AppCommand>,
) {
    let area = ui.available_rect_before_wrap();
    let cell_width = (area.width() - GRID_GAP * (GRID_COLUMNS as f32 - 1.0)) / GRID_COLUMNS as f32;
    // Covers are 2:3 posters; shrink a little if needed so two full rows fit on screen.
    let cell_height = (cell_width * 1.5)
        .min((area.height() - GRID_GAP) / 2.0)
        .max(40.0);
    let row_pitch = cell_height + GRID_GAP;
    let rows = grid.len().div_ceil(GRID_COLUMNS);
    let content_height = rows as f32 * row_pitch - GRID_GAP;
    let max_offset = (content_height - area.height()).max(0.0);

    let offset_id = egui::Id::new("title_grid_offset");
    let last_selected_id = egui::Id::new("title_grid_last_selected");
    let mut offset: f32 = ui.data(|d| d.get_temp(offset_id)).unwrap_or(0.0);
    let last_selected: Option<Option<usize>> = ui.data(|d| d.get_temp(last_selected_id));

    // `click_and_drag` needs real movement before dragging, so it doesn't fight taps.
    let drag = ui.interact(
        area,
        egui::Id::new("title_grid_drag"),
        egui::Sense::click_and_drag(),
    );
    offset -= drag.drag_delta().y;

    if last_selected != Some(selected_position)
        && let Some(position) = selected_position
    {
        let top = (position / GRID_COLUMNS) as f32 * row_pitch;
        let bottom = top + cell_height;
        if top < offset {
            offset = top;
        } else if bottom > offset + area.height() {
            offset = bottom - area.height();
        }
    }
    let offset = offset.clamp(0.0, max_offset);
    ui.data_mut(|d| {
        d.insert_temp(offset_id, offset);
        d.insert_temp(last_selected_id, selected_position);
    });

    let first_row = (offset / row_pitch).floor() as usize;
    let last_row = (((offset + area.height()) / row_pitch).ceil() as usize).min(rows);
    let visible = (
        first_row * GRID_COLUMNS,
        (last_row * GRID_COLUMNS).min(grid.len()),
    );
    if visible != app.title_grid_visible {
        commands.push(Command::SetVisible(visible.0, visible.1).into());
    }

    let painter = ui.painter_at(area);
    for position in visible.0..visible.1 {
        let Some(title) = app.service.titles.get(grid[position]) else {
            continue;
        };
        let row = position / GRID_COLUMNS;
        let column = position % GRID_COLUMNS;
        let rect = egui::Rect::from_min_size(
            egui::pos2(
                area.min.x + column as f32 * (cell_width + GRID_GAP),
                area.min.y + row as f32 * row_pitch - offset,
            ),
            egui::vec2(cell_width, cell_height),
        );
        let is_selected = selected_position == Some(position);
        draw_grid_cell(ui, &painter, rect, title, is_selected, theme);

        let hit_rect = rect.intersect(area);
        if hit_rect.is_positive() {
            let response = ui.interact(
                hit_rect,
                egui::Id::new(("title_grid_cell", grid[position])),
                egui::Sense::click(),
            );
            if response.clicked() {
                if is_selected {
                    commands.push(Command::OpenDetails.into());
                } else {
                    commands.push(Command::Select(grid[position]).into());
                }
            }
        }
    }
    ui.advance_cursor_after_rect(area);
}

fn draw_grid_cell(
    ui: &egui::Ui,
    painter: &egui::Painter,
    rect: egui::Rect,
    title: &Game,
    is_selected: bool,
    theme: Theme,
) {
    const RADIUS: f32 = 6.0;
    painter.rect_filled(rect, RADIUS, egui::Color32::from_rgb(0x26, 0x27, 0x2c));
    let name = title.display_name();
    match title.thumb.as_ref() {
        Some(thumb) => draw_cover(ui, painter, thumb, rect),
        None => {
            // No cover yet (loading, or the game has none): show the name instead.
            let galley = layout_name(painter, name, rect.width() - 10.0, 4, 12.0, theme.text);
            let pos = rect.center() - galley.rect.center().to_vec2();
            painter.galley(pos, galley, theme.text);
        }
    }

    if is_selected {
        // Name banner at the bottom of the highlighted cover.
        if title.thumb.is_some() {
            let galley = layout_name(
                painter,
                name,
                rect.width() - 8.0,
                2,
                12.0,
                egui::Color32::WHITE,
            );
            let banner_height = galley.size().y + 8.0;
            let banner = egui::Rect::from_min_max(
                egui::pos2(rect.min.x, rect.max.y - banner_height),
                rect.max,
            );
            painter.rect_filled(
                banner,
                egui::CornerRadius {
                    nw: 0,
                    ne: 0,
                    sw: RADIUS as u8,
                    se: RADIUS as u8,
                },
                egui::Color32::from_black_alpha(200),
            );
            // Centered text: the galley's own rect is centered on x = 0, so place by its rect.
            let pos = banner.center() - galley.rect.center().to_vec2();
            painter.galley(pos, galley, egui::Color32::WHITE);
        }
        painter.rect_stroke(
            rect.expand(1.5),
            RADIUS + 1.0,
            egui::Stroke::new(3.0_f32, theme.accent),
            egui::StrokeKind::Outside,
        );
        painter.rect_stroke(
            rect.expand(4.5),
            RADIUS + 4.0,
            egui::Stroke::new(1.5_f32, egui::Color32::WHITE),
            egui::StrokeKind::Outside,
        );
    }
}

/// Fills `rect` with a 2:3 poster (cropping the edges a little if needed); a square box art is
/// fitted whole instead, so its title is not cut off.
fn draw_cover(ui: &egui::Ui, painter: &egui::Painter, image: &Arc<TitleImage>, rect: egui::Rect) {
    let texture = image.texture(ui.ctx(), "grid-cover");
    let image_aspect = image.width as f32 / image.height as f32;
    let rect_aspect = rect.width() / rect.height();
    let full_uv = egui::Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1.0, 1.0));
    if image_aspect > 0.85 {
        let height = rect.width() / image_aspect;
        let fitted = egui::Rect::from_center_size(rect.center(), egui::vec2(rect.width(), height));
        painter.image(texture.id(), fitted, full_uv, egui::Color32::WHITE);
        return;
    }
    let uv = if image_aspect > rect_aspect {
        let inset = (1.0 - rect_aspect / image_aspect) * 0.5;
        egui::Rect::from_min_max(egui::pos2(inset, 0.0), egui::pos2(1.0 - inset, 1.0))
    } else {
        let inset = (1.0 - image_aspect / rect_aspect) * 0.5;
        egui::Rect::from_min_max(egui::pos2(0.0, inset), egui::pos2(1.0, 1.0 - inset))
    };
    painter.image(texture.id(), rect, uv, egui::Color32::WHITE);
}

fn layout_name(
    painter: &egui::Painter,
    name: &str,
    width: f32,
    max_rows: usize,
    size: f32,
    color: egui::Color32,
) -> Arc<egui::Galley> {
    let mut job = egui::text::LayoutJob::single_section(
        name.to_owned(),
        egui::TextFormat::simple(egui::FontId::proportional(size), color),
    );
    job.wrap = egui::text::TextWrapping {
        max_width: width.max(10.0),
        max_rows,
        break_anywhere: false,
        overflow_character: Some('\u{2026}'),
    };
    job.halign = egui::Align::Center;
    painter.layout_job(job)
}

/// Details page of the highlighted game: cover, info, Play button and description.
fn show_details(
    ui: &mut egui::Ui,
    app: &App,
    title: &Game,
    theme: Theme,
    i18n: &I18n,
    commands: &mut Vec<AppCommand>,
) {
    let _ = app;
    // The description scrolls; the rest of the page stays put.
    egui::Frame::NONE
        .inner_margin(egui::Margin {
            left: 18,
            right: 18,
            top: 0,
            bottom: 0,
        })
        .show(ui, |ui| {
            const COVER_WIDTH: f32 = 140.0;
            let catalog = title.details.as_ref();
            let loading = catalog.is_none();
            // While the highlighted title's cover is loading, reserve the last cover's rendered height
            // rather than a generic guess - box art mostly shares the same aspect ratio.
            let last_cover_height_id = egui::Id::new("title_last_cover_height");
            let cover_height = match title.box_art.as_ref() {
                Some(box_art) => {
                    let height = COVER_WIDTH * box_art.height as f32 / box_art.width as f32;
                    ui.ctx()
                        .data_mut(|d| d.insert_temp(last_cover_height_id, height));
                    height
                }
                None => ui
                    .ctx()
                    .data(|d| d.get_temp(last_cover_height_id))
                    .unwrap_or(160.0),
            };

            ui.horizontal(|ui| {
                let (rect, _) = ui.allocate_exact_size(
                    egui::vec2(COVER_WIDTH, cover_height),
                    egui::Sense::hover(),
                );
                if let Some(box_art) = title.box_art.as_ref() {
                    draw_title_image(ui, box_art, rect, "box-art");
                } else {
                    ui.painter()
                        .rect_filled(rect, 6.0, egui::Color32::from_rgb(0x18, 0x19, 0x1c));
                    if loading {
                        ui.put(rect, egui::Spinner::new());
                    }
                }

                ui.add_space(12.0);

                // Fixed to `cover_height` so the bottom_up Play button lands flush with the cover art.
                ui.allocate_ui_with_layout(
                    egui::vec2(ui.available_width(), cover_height),
                    egui::Layout::top_down(egui::Align::Min),
                    |ui| {
                        ui.heading(
                            egui::RichText::new(title.display_name()).color(theme.text_bright),
                        );
                        ui.add_space(6.0);
                        let mut metadata = catalog
                            .map(|catalog| catalog.genres.clone())
                            .unwrap_or_default();
                        if let Some(year) = catalog
                            .and_then(|catalog| catalog.release_date.as_deref())
                            .and_then(|date| date.get(0..4))
                        {
                            metadata.push(year.to_owned());
                        }
                        if let Some(rating) = catalog.and_then(|catalog| catalog.average_rating) {
                            metadata.push(match catalog.and_then(|catalog| catalog.rating_count) {
                                Some(count) => format!("\u{2605} {rating:.1} ({count})"),
                                None => format!("\u{2605} {rating:.1}"),
                            });
                        }
                        if let Some(content_rating) =
                            catalog.and_then(|catalog| catalog.content_rating.as_ref())
                        {
                            metadata.push(content_rating.clone());
                        }
                        if !metadata.is_empty() {
                            ui.colored_label(theme.text_bright, metadata.join(" \u{b7} "));
                            ui.add_space(4.0);
                        }
                        let credits = catalog.and_then(|catalog| {
                            match (&catalog.developer, &catalog.publisher) {
                                (Some(developer), Some(publisher)) if developer == publisher => {
                                    Some(developer.clone())
                                }
                                (Some(developer), Some(publisher)) => {
                                    Some(format!("{developer} \u{b7} {publisher}"))
                                }
                                (Some(developer), None) => Some(developer.clone()),
                                (None, Some(publisher)) => Some(publisher.clone()),
                                (None, None) => None,
                            }
                        });
                        if let Some(credits) = credits {
                            ui.colored_label(theme.text, credits);
                        }

                        ui.with_layout(egui::Layout::bottom_up(egui::Align::Min), |ui| {
                            // `bottom_up` places the first widget at the bottom.
                            ui.add_space(10.0);

                            let play_button = egui::Button::new(
                                egui::RichText::new(i18n.text("title-play"))
                                    .size(20.0)
                                    .strong()
                                    .color(egui::Color32::WHITE),
                            )
                            .fill(theme.accent)
                            .corner_radius(8.0);
                            let button_width = ui.available_width();
                            if ui
                                .add_sized(egui::vec2(button_width, 44.0), play_button)
                                .clicked()
                            {
                                commands.push(InputCommand::Confirm.into());
                            }
                            ui.add_space(6.0);
                            let back = egui::Button::new(
                                egui::RichText::new(format!(
                                    "\u{25cb} {}",
                                    i18n.text("action-back")
                                ))
                                .size(15.0)
                                .color(theme.text_bright),
                            )
                            .fill(egui::Color32::from_rgb(0x26, 0x27, 0x2c))
                            .corner_radius(8.0);
                            if ui.add_sized(egui::vec2(button_width, 32.0), back).clicked() {
                                commands.push(Command::CloseDetails.into());
                            }
                        });
                    },
                );
            });

            ui.add_space(10.0);

            egui::ScrollArea::vertical()
                .id_salt("title_details_description")
                .show(ui, |ui| {
                    match catalog.and_then(|catalog| catalog.description.as_ref()) {
                        Some(description) => {
                            ui.colored_label(theme.text, description);
                        }
                        None if !loading => {
                            ui.colored_label(theme.text, i18n.text("title-no-description"));
                        }
                        None => {}
                    }
                });
        });
}

#[derive(Clone)]
struct BackgroundFade {
    current_game_id: String,
    current: Arc<TitleImage>,
    previous: Option<Arc<TitleImage>>,
    pending: Option<(String, Arc<TitleImage>)>,
    changed_at: Instant,
}

pub(crate) fn draw_title_background(ui: &mut egui::Ui, app: &App, theme: Theme) {
    let Some(title) = app.highlighted_title() else {
        return;
    };
    if let Some(background) = title.background.as_ref() {
        background.texture(ui.ctx(), "title-background");
    }
    let fade_id = egui::Id::new("title_background_fade");
    let fade = ui.ctx().data_mut(|data| {
        let existing = data.get_temp::<BackgroundFade>(fade_id);
        let fade = match (title.background.as_ref(), existing) {
            (Some(background), Some(mut fade)) => {
                if fade.current_game_id == title.id {
                    fade.pending = None;
                } else if fade
                    .pending
                    .as_ref()
                    .is_some_and(|(game_id, _)| game_id == &title.id)
                {
                    let (_, background) = fade
                        .pending
                        .take()
                        .expect("pending background checked above");
                    fade.previous = Some(std::mem::replace(&mut fade.current, background));
                    fade.current_game_id.clone_from(&title.id);
                    fade.changed_at = Instant::now();
                } else {
                    fade.pending = Some((title.id.clone(), Arc::clone(background)));
                }
                Some(fade)
            }
            (Some(background), None) => Some(BackgroundFade {
                current_game_id: title.id.clone(),
                current: Arc::clone(background),
                previous: None,
                pending: None,
                changed_at: Instant::now(),
            }),
            (None, Some(mut fade)) => {
                fade.pending = None;
                Some(fade)
            }
            (None, None) => None,
        };
        if let Some(fade) = &fade {
            data.insert_temp(fade_id, fade.clone());
        }
        fade
    });
    let Some(mut fade) = fade else {
        return;
    };

    let rect = ui.ctx().screen_rect();
    // The painter uploads the pending texture now, while the old background remains visible.
    // Only the following UI frame promotes it and starts the crossfade timer.
    if let Some((_, pending)) = &fade.pending {
        draw_title_image_cover(
            ui,
            pending,
            rect,
            "title-background-pending",
            egui::Color32::from_rgba_unmultiplied(255, 255, 255, 1),
        );
        ui.ctx().request_repaint();
    }

    const FADE_DURATION: Duration = Duration::from_millis(420);
    const IMAGE_ALPHA: u8 = 150;
    let linear_t =
        (fade.changed_at.elapsed().as_secs_f32() / FADE_DURATION.as_secs_f32()).clamp(0.0, 1.0);
    let fade_t = linear_t * linear_t * (3.0 - 2.0 * linear_t);

    if linear_t >= 1.0 && fade.previous.take().is_some() {
        ui.ctx()
            .data_mut(|data| data.insert_temp(fade_id, fade.clone()));
    } else if fade.previous.is_some() {
        ui.ctx().request_repaint_after(Duration::from_millis(16));
    }

    let current_alpha = if fade.previous.is_some() {
        (fade_t * IMAGE_ALPHA as f32).round() as u8
    } else {
        IMAGE_ALPHA
    };

    if let Some(previous) = &fade.previous {
        let alpha = ((1.0 - fade_t) * IMAGE_ALPHA as f32).round() as u8;
        if alpha > 0 {
            draw_title_image_cover(
                ui,
                previous,
                rect,
                "title-background-previous",
                egui::Color32::from_rgba_unmultiplied(255, 255, 255, alpha),
            );
        }
    }
    draw_title_image_cover(
        ui,
        &fade.current,
        rect,
        "title-background",
        egui::Color32::from_rgba_unmultiplied(255, 255, 255, current_alpha),
    );
    ui.painter().rect_filled(
        rect,
        0.0,
        egui::Color32::from_rgba_unmultiplied(
            theme.background.r(),
            theme.background.g(),
            theme.background.b(),
            120,
        ),
    );
}

fn move_in_grid(position: usize, len: usize, command: InputCommand) -> Option<usize> {
    let columns = GRID_COLUMNS;
    match command {
        InputCommand::MoveLeft => position.checked_sub(1),
        InputCommand::MoveRight => (position + 1 < len).then_some(position + 1),
        InputCommand::MoveUp => position.checked_sub(columns),
        InputCommand::MoveDown if position + columns < len => Some(position + columns),
        // From a full row onto a shorter last row: land on the last game.
        InputCommand::MoveDown if position / columns < (len - 1) / columns => Some(len - 1),
        _ => None,
    }
}

impl App {
    fn select_title(&mut self, index: usize) {
        if let AppState::TitleList { selected } = &mut self.state {
            *selected = index;
        }
    }

    pub(crate) async fn handle_title_list_input(&mut self, command: InputCommand) -> Result<()> {
        let AppState::TitleList { selected } = &self.state else {
            if command == InputCommand::Back {
                self.set_state(AppState::ModeSelect { selected: 0 });
            }
            return Ok(());
        };
        let selected = *selected;
        let grid = grid_title_indices(self);
        let current = grid.iter().position(|index| *index == selected);

        if self.title_details_open {
            match command {
                InputCommand::Confirm => self.start_selected_title(selected),
                InputCommand::Back => self.title_details_open = false,
                InputCommand::MoveLeft | InputCommand::MoveRight => {
                    let target = current.and_then(|position| match command {
                        InputCommand::MoveLeft => position.checked_sub(1),
                        _ => (position + 1 < grid.len()).then_some(position + 1),
                    });
                    if let Some(target) = target {
                        self.select_title(grid[target]);
                    }
                }
                _ => {}
            }
            return Ok(());
        }

        match command {
            InputCommand::MoveUp
            | InputCommand::MoveDown
            | InputCommand::MoveLeft
            | InputCommand::MoveRight => match current {
                Some(position) => {
                    if let Some(target) = move_in_grid(position, grid.len(), command) {
                        self.select_title(grid[target]);
                    }
                }
                None => {
                    if let Some(first) = grid.first() {
                        self.select_title(*first);
                    }
                }
            },
            InputCommand::Confirm => {
                if current.is_some() {
                    self.title_details_open = true;
                }
            }
            InputCommand::Back => {
                if searching(self) {
                    self.set_title_search_query(String::new());
                } else {
                    self.set_state(AppState::ModeSelect { selected: 0 });
                }
            }
            InputCommand::PrevPage | InputCommand::NextPage => {
                if !searching(self) {
                    let count = categories(self).len();
                    if count > 1 {
                        let next = if command == InputCommand::NextPage {
                            (self.title_category + 1) % count
                        } else {
                            (self.title_category + count - 1) % count
                        };
                        self.set_title_category(next);
                    }
                }
            }
            InputCommand::Search => self.title_search_requested = true,
        }

        Ok(())
    }

    fn start_selected_title(&mut self, selected: usize) {
        let Some(title) = self.service.titles.get(selected).cloned() else {
            return;
        };
        let game_id = title.id;
        let target_id = title.launch_id;
        self.note_title_played(&game_id);
        self.start_stream_for_target(StreamStartTarget {
            kind: StreamKind::Cloud,
            label: format!("cloud title {target_id}"),
            target_id,
            game_id: Some(game_id),
            return_selected: selected,
        });
    }

    pub(crate) fn current_category_key(&self) -> &'static str {
        let categories = categories(self);
        categories
            .get(self.title_category.min(categories.len().saturating_sub(1)))
            .map_or("all", |category| category.key)
    }

    /// Points `title_category` back at the tab named `key` after the tab list changed.
    pub(crate) fn restore_category(&mut self, key: &'static str) {
        if let Some(index) = categories(self)
            .iter()
            .position(|category| category.key == key)
        {
            self.title_category = index;
        }
    }

    fn set_title_category(&mut self, category: usize) {
        self.title_category = category;
        self.title_grid_visible = (0, 0);
        if let Some(first) = grid_title_indices(self).first() {
            self.select_title(*first);
        }
    }

    pub(crate) fn handle_title_list_command(&mut self, command: Command) -> Result<()> {
        match command {
            Command::OpenSearch => self.title_search_requested = true,
            Command::SetSearch(query) => self.set_title_search_query(query),
            Command::Select(index) => self.select_title(index),
            Command::OpenDetails => {
                if matches!(&self.state, AppState::TitleList { .. }) {
                    self.title_details_open = true;
                }
            }
            Command::CloseDetails => self.title_details_open = false,
            Command::SelectCategory(category) => self.set_title_category(category),
            Command::SetVisible(start, end) => self.title_grid_visible = (start, end),
        }
        Ok(())
    }

    pub(crate) fn set_title_search_query(&mut self, query: String) {
        self.title_search_query = query;
        self.title_grid_visible = (0, 0);
        let grid = grid_title_indices(self);
        if let Some(first) = grid.first()
            && let AppState::TitleList { selected } = &mut self.state
            && !grid.contains(selected)
        {
            *selected = *first;
        }
    }
}

#[cfg(test)]
mod search_tests {
    use super::contains_case_insensitive;

    #[test]
    fn search_is_case_insensitive_and_matches_substrings() {
        assert!(contains_case_insensitive("Forza Horizon 5", "HORIZON"));
        assert!(contains_case_insensitive("Été", "été"));
        assert!(!contains_case_insensitive("Halo Infinite", "Forza"));
    }
}
