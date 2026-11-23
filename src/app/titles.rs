use super::image::{TitleImage, mask_to_circle};
use super::ui::screens::title_list::{GRID_COLUMNS, grid_title_indices};
use super::{App, AppState};
use crate::api::catalog::worker::{CatalogResult, ImageKind, clear_catalog_cache};
use crate::api::catalog::{Game, GameDetails};
use std::sync::Arc;

const RESULTS_PER_TICK: usize = 4;
/// Most grid covers kept in memory at once (about 100 KB each).
pub(crate) const THUMB_CACHE_LIMIT: usize = 36;
/// Titles asked per catalog request.
const METADATA_BATCH_SIZE: usize = 12;
/// Pause after a failed catalog request, so a dead 4G link is not hammered.
const METADATA_RETRY_DELAY: std::time::Duration = std::time::Duration::from_secs(5);
/// Cover and background are only fetched/decoded once a title stays highlighted this long, so
/// holding the D-pad through the list does not queue dozens of large images.
const ART_SETTLE_DELAY: std::time::Duration = std::time::Duration::from_millis(350);

impl App {
    /// The pending-request set matching `kind`, so a fetch isn't re-queued while in flight.
    fn image_pending_set(&mut self, kind: ImageKind) -> &mut std::collections::HashSet<String> {
        match kind {
            ImageKind::Cover => &mut self.service.box_art_pending,
            ImageKind::Background => &mut self.service.background_pending,
            ImageKind::Thumb => &mut self.service.thumb_pending,
            ImageKind::Avatar => unreachable!("avatar requests aren't tracked per title"),
        }
    }

    fn request_title_image(
        &mut self,
        title_id: String,
        kind: ImageKind,
        url: String,
        prefetch: bool,
    ) {
        let cache_locale = self.settings.locale.as_str().to_lowercase();
        let queued = if prefetch {
            self.service.catalog_worker.prefetch_image(
                title_id.clone(),
                kind,
                url,
                Some(cache_locale),
            )
        } else {
            self.service.catalog_worker.request_image(
                title_id.clone(),
                kind,
                url,
                Some(cache_locale),
            )
        };
        if queued {
            self.image_pending_set(kind).insert(title_id);
        }
    }

    /// Fetches metadata + cover art/background for the highlighted title via `CatalogWorker`.
    fn ensure_title_details_job(&mut self) {
        let AppState::TitleList { selected } = &self.state else {
            return;
        };
        if !self.title_details_open {
            return;
        }
        let selected = *selected;
        let Some(title) = self.service.titles.get(selected) else {
            return;
        };
        let title_id = title.id.clone();

        if title.details.is_none() {
            self.request_metadata_if_needed(&title_id, title.metadata_id.clone(), false);
            return;
        }

        let now = std::time::Instant::now();
        match self.service.highlighted_since {
            Some((index, since)) if index == selected => {
                if now.duration_since(since) < ART_SETTLE_DELAY {
                    return;
                }
            }
            _ => {
                self.service.highlighted_since = Some((selected, now));
                return;
            }
        }

        let title = &self.service.titles[selected];
        let details = title.details.as_ref().expect("title details checked above");
        let box_art_url = (title.box_art.is_none()
            && !self.service.box_art_pending.contains(&title_id))
        .then(|| details.box_art_url.clone())
        .flatten();
        let background_url = (title.background.is_none()
            && !self.service.background_pending.contains(&title_id))
        .then(|| details.background_url.clone())
        .flatten();

        if let Some(url) = box_art_url {
            self.request_title_image(title_id.clone(), ImageKind::Cover, url, false);
        }
        if let Some(url) = background_url {
            self.request_title_image(title_id, ImageKind::Background, url, false);
        }
    }

    /// Grid positions (in the shown list) whose metadata and covers should be loaded: the rows
    /// on screen plus one row above and two below, nearest to the selection first.
    fn grid_wanted_positions(&self, grid: &[usize]) -> Vec<usize> {
        let columns = GRID_COLUMNS;
        let (visible_start, visible_end) = self.title_grid_visible;
        let selected_position = match &self.state {
            AppState::TitleList { selected } => grid.iter().position(|index| index == selected),
            _ => None,
        }
        .unwrap_or(visible_start);
        // Before the first frame reports what is on screen, assume the first rows.
        let visible_end = if visible_end <= visible_start {
            visible_start + columns * 3
        } else {
            visible_end
        };
        let start = visible_start.saturating_sub(columns);
        let end = (visible_end + columns * 2).min(grid.len());
        let mut positions: Vec<usize> = (start..end).collect();
        positions.sort_by_key(|position| {
            let on_screen = (visible_start..visible_end).contains(position);
            (!on_screen, position.abs_diff(selected_position))
        });
        positions.truncate(THUMB_CACHE_LIMIT);
        positions
    }

    fn ensure_grid_jobs(&mut self) {
        if !matches!(&self.state, AppState::TitleList { .. }) {
            return;
        }
        let grid = grid_title_indices(self);
        let wanted = self.grid_wanted_positions(&grid);

        // Metadata (name + cover URL), several titles per request.
        let now = std::time::Instant::now();
        if self
            .service
            .metadata_retry_after
            .is_none_or(|after| now >= after)
        {
            let pending = &self.service.title_detail_pending;
            let batch: Vec<(String, String)> = wanted
                .iter()
                .filter_map(|position| self.service.titles.get(grid[*position]))
                .filter(|title| title.details.is_none() && !pending.contains(&title.id))
                .filter_map(|title| Some((title.id.clone(), title.metadata_id.clone()?)))
                .take(METADATA_BATCH_SIZE)
                .collect();
            if !batch.is_empty() {
                let ids: Vec<String> = batch.iter().map(|(id, _)| id.clone()).collect();
                if self.service.catalog_worker.request_metadata_batch(
                    batch,
                    self.settings.locale.market().to_owned(),
                    self.settings.locale.as_str().to_lowercase(),
                ) {
                    self.service.title_detail_pending.extend(ids);
                }
            }
        }
        // Titles without a store id never get metadata: give them empty details.
        for position in &wanted {
            if let Some(title) = self.service.titles.get_mut(grid[*position])
                && title.details.is_none()
                && title.metadata_id.is_none()
            {
                title.details = Some(GameDetails::default());
            }
        }

        // Covers, nearest first, while the worker queue has room.
        let requests: Vec<(String, String)> = wanted
            .iter()
            .filter_map(|position| self.service.titles.get(grid[*position]))
            .filter(|title| {
                title.thumb.is_none() && !self.service.thumb_pending.contains(&title.id)
            })
            .filter_map(|title| Some((title.id.clone(), title.thumb_url()?)))
            .take(4)
            .collect();
        for (title_id, url) in requests {
            let before = self.service.thumb_pending.len();
            self.request_title_image(title_id, ImageKind::Thumb, url, true);
            if self.service.thumb_pending.len() == before {
                break;
            }
        }
    }

    /// Frees covers that scrolled away, and the large art once the details page is closed.
    fn prune_title_images(&mut self) {
        let AppState::TitleList { selected } = &self.state else {
            return;
        };
        let selected = *selected;
        let details_open = self.title_details_open;
        let grid = grid_title_indices(self);
        let keep: std::collections::HashSet<usize> = self
            .grid_wanted_positions(&grid)
            .into_iter()
            .map(|position| grid[position])
            .collect();
        for (index, title) in self.service.titles.iter_mut().enumerate() {
            if index != selected || !details_open {
                title.box_art = None;
                title.background = None;
            }
            if !keep.contains(&index) {
                title.thumb = None;
            }
        }
    }

    fn request_metadata_if_needed(
        &mut self,
        title_id: &str,
        metadata_id: Option<String>,
        prefetch: bool,
    ) {
        if self.service.title_detail_pending.contains(title_id) {
            return;
        }
        let Some(metadata_id) = metadata_id else {
            if let Some(title) = self
                .service
                .titles
                .iter_mut()
                .find(|title| title.id == title_id)
            {
                title.details = Some(GameDetails::default());
            }
            return;
        };

        let title_id = title_id.to_owned();
        let queued = if prefetch {
            self.service.catalog_worker.prefetch_metadata(
                title_id.clone(),
                metadata_id,
                self.settings.locale.market().to_owned(),
                self.settings.locale.as_str().to_lowercase(),
            )
        } else {
            self.service.catalog_worker.request_metadata(
                title_id.clone(),
                metadata_id,
                self.settings.locale.market().to_owned(),
                self.settings.locale.as_str().to_lowercase(),
            )
        };
        if queued {
            self.service.title_detail_pending.insert(title_id);
        }
    }

    pub(super) async fn pump_title_details(&mut self) -> anyhow::Result<()> {
        let now = std::time::Instant::now();
        if self
            .service
            .last_memory_log
            .is_none_or(|last| now.duration_since(last) >= std::time::Duration::from_secs(20))
        {
            self.service.last_memory_log = Some(now);
            let selected = match &self.state {
                AppState::TitleList { selected } => *selected,
                _ => 0,
            };
            crate::diag_log::log(
                "memory",
                format!(
                    "game list: title {}/{} covers in memory {} {}",
                    selected + 1,
                    self.service.titles.len(),
                    self.service
                        .titles
                        .iter()
                        .filter(|title| title.thumb.is_some())
                        .count(),
                    crate::streaming::video::free_memory_summary()
                ),
            );
        }
        self.prune_title_images();
        self.ensure_title_details_job();
        self.ensure_grid_jobs();

        for _ in 0..RESULTS_PER_TICK {
            let Some(result) = self.service.catalog_worker.try_recv() else {
                break;
            };
            match result {
                CatalogResult::Metadata { game_id, details } => {
                    self.service.title_detail_pending.remove(&game_id);
                    if let Some(title) = self
                        .service
                        .titles
                        .iter_mut()
                        .find(|title| title.id == game_id)
                    {
                        title.details = Some(details.unwrap_or_default());
                    }
                }
                CatalogResult::MetadataFailed { game_ids } => {
                    for game_id in &game_ids {
                        self.service.title_detail_pending.remove(game_id);
                    }
                    self.service.metadata_retry_after =
                        Some(std::time::Instant::now() + METADATA_RETRY_DELAY);
                }
                CatalogResult::Image { game_id, kind, art } => {
                    let image = art.map(|(rgba, width, height)| {
                        let mut image = TitleImage::new(rgba, width, height);
                        if kind == ImageKind::Avatar {
                            mask_to_circle(&mut image);
                        }
                        Arc::new(image)
                    });
                    if kind == ImageKind::Avatar {
                        self.service.avatar = image;
                        continue;
                    }
                    self.image_pending_set(kind).remove(&game_id);
                    let selected = match &self.state {
                        AppState::TitleList { selected } => *selected,
                        _ => continue,
                    };
                    let keep_thumb = kind == ImageKind::Thumb && {
                        let grid = grid_title_indices(self);
                        self.grid_wanted_positions(&grid)
                            .into_iter()
                            .any(|position| self.service.titles[grid[position]].id == game_id)
                    };
                    if let Some((index, title)) = self
                        .service
                        .titles
                        .iter_mut()
                        .enumerate()
                        .find(|(_, title)| title.id == game_id)
                    {
                        match kind {
                            ImageKind::Cover if index == selected => title.box_art = image,
                            ImageKind::Background if index == selected => title.background = image,
                            ImageKind::Thumb if keep_thumb => title.thumb = image,
                            ImageKind::Cover | ImageKind::Background | ImageKind::Thumb => {}
                            ImageKind::Avatar => unreachable!(),
                        }
                    }
                }
            }
        }

        Ok(())
    }

    pub(crate) fn highlighted_title(&self) -> Option<&Game> {
        let AppState::TitleList { selected } = &self.state else {
            return None;
        };
        self.service.titles.get(*selected)
    }

    pub(super) fn invalidate_catalog_for_locale_change(&mut self) {
        if let Err(error) = clear_catalog_cache() {
            eprintln!("Title cache: failed to clear after locale change: {error:#}");
        }
        for title in &mut self.service.titles {
            title.details = None;
            title.thumb = None;
            title.box_art = None;
            title.background = None;
        }
        self.service.restart_catalog_worker();
    }
}
