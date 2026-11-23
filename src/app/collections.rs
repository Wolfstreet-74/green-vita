//! Game grid categories: recently played (this Vita + the Xbox server) and Game Pass lists
//! (new, leaving soon, popular) from the public `catalog.gamepass.com` "sigls" endpoint.
//!
//! The sigl list ids are not documented by Microsoft. Each one is tried at load time; a list
//! that fails or has no cloud game in common with the user's library is simply not shown.
//! The tab label is the list title returned by the server, already in the user's language.

use super::{App, AppState};
use crate::jobs::{PollJob, poll_job};
use crate::{ApiClient, StreamKind};
use anyhow::{Context, Result};
use std::time::Duration;

/// Candidate Game Pass lists, in tab order: `(key, sigl id)`.
const SIGL_LISTS: [(&str, &str); 4] = [
    // "Most popular" - unconfirmed ids, kept only if the server answers.
    ("popular", "eab7757c-ff70-45af-bfa6-79d4cfb5e7a8"),
    ("popular-2", "e7590b22-e299-44db-ae22-25c61405454c"),
    // "Recently added".
    ("new", "f13cf6b4-57e6-4459-89df-6aec18cf0538"),
    // "Leaving soon" (console list).
    ("leaving", "393f05bf-e596-4ef6-9487-6d4fa0eab987"),
];
/// Most games kept in "recently played" (this Vita's history, then the server's).
pub(crate) const RECENT_LIMIT: usize = 24;

/// A Game Pass list as received: its title and the Store product ids it holds.
pub(crate) struct FetchedList {
    key: &'static str,
    title: String,
    product_ids: Vec<String>,
}

pub(crate) struct FetchedCollections {
    lists: Vec<FetchedList>,
    /// xCloud "most recently used" title ids, newest first.
    recent_title_ids: Vec<String>,
}

/// A Game Pass list matched against the user's cloud library.
pub(crate) struct Collection {
    pub(crate) key: &'static str,
    pub(crate) title: String,
    /// Indices into `service.titles`, in the list's order.
    pub(crate) indices: Vec<usize>,
}

async fn fetch_collections(
    api: ApiClient,
    market: String,
    language: String,
) -> Result<FetchedCollections> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap_or_default();

    let mut lists = Vec::new();
    for (key, sigl_id) in SIGL_LISTS {
        match fetch_sigl(&client, sigl_id, &market, &language).await {
            Ok((title, product_ids)) => {
                crate::diag_log::log(
                    "catalog",
                    format!("list {key}: \"{title}\" with {} games", product_ids.len()),
                );
                lists.push(FetchedList {
                    key,
                    title,
                    product_ids,
                });
            }
            Err(error) => {
                crate::diag_log::log("catalog", format!("list {key} unavailable: {error:#}"));
            }
        }
    }

    let recent_title_ids = match api
        .get_json::<serde_json::Value>(StreamKind::Cloud, "/v2/titles/mru?mr=25")
        .await
    {
        Ok(value) => {
            let ids: Vec<String> = value["results"]
                .as_array()
                .map(|results| {
                    results
                        .iter()
                        .filter_map(|entry| entry["titleId"].as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default();
            crate::diag_log::log(
                "catalog",
                format!("recently played on server: {}", ids.len()),
            );
            ids
        }
        Err(error) => {
            crate::diag_log::log("catalog", format!("recently played unavailable: {error:#}"));
            Vec::new()
        }
    };

    Ok(FetchedCollections {
        lists,
        recent_title_ids,
    })
}

/// Returns the list title and its product ids. The answer is a JSON array: a header object
/// (`siglId`, `title`, ...) followed by `{"id": "<productId>"}` entries.
async fn fetch_sigl(
    client: &reqwest::Client,
    sigl_id: &str,
    market: &str,
    language: &str,
) -> Result<(String, Vec<String>)> {
    let url = format!(
        "https://catalog.gamepass.com/sigls/v2?id={sigl_id}&language={language}&market={market}"
    );
    let value: serde_json::Value = client
        .get(&url)
        .send()
        .await
        .context("request failed")?
        .error_for_status()
        .context("error status")?
        .json()
        .await
        .context("invalid answer")?;
    let entries = value.as_array().context("answer is not a list")?;
    let title = entries
        .iter()
        .find_map(|entry| entry["title"].as_str())
        .unwrap_or_default()
        .trim()
        .to_owned();
    let product_ids: Vec<String> = entries
        .iter()
        .filter_map(|entry| entry["id"].as_str().map(str::to_owned))
        .collect();
    anyhow::ensure!(!title.is_empty(), "list has no title");
    anyhow::ensure!(!product_ids.is_empty(), "list is empty");
    Ok((title, product_ids))
}

impl App {
    /// Starts loading the category lists, once the cloud library is known.
    pub(super) fn start_collections_job(&mut self) {
        let api = self.service.api.clone();
        let market = self.settings.locale.market().to_owned();
        let language = self.settings.locale.as_str().to_lowercase();
        self.service.collections.clear();
        self.service.server_recent_title_ids.clear();
        self.service.collections_job = Some(tokio::spawn(fetch_collections(api, market, language)));
    }

    pub(super) async fn pump_collections_job(&mut self) {
        let Some(job) = self.service.collections_job.take() else {
            return;
        };
        let fetched = match poll_job(job).await {
            PollJob::Pending(job) => {
                self.service.collections_job = Some(job);
                return;
            }
            PollJob::Done(Ok(fetched)) => fetched,
            PollJob::Done(Err(error)) => {
                crate::diag_log::log("catalog", format!("category lists failed: {error:#}"));
                return;
            }
        };

        let titles = &self.service.titles;
        let mut collections: Vec<Collection> = Vec::new();
        for list in fetched.lists {
            // Two candidate ids can turn out to be the same list.
            if collections
                .iter()
                .any(|collection| collection.title == list.title)
            {
                continue;
            }
            let mut indices = Vec::new();
            for product_id in &list.product_ids {
                if let Some(index) = titles.iter().position(|title| {
                    title
                        .metadata_id
                        .as_deref()
                        .is_some_and(|id| id.eq_ignore_ascii_case(product_id))
                }) && !indices.contains(&index)
                {
                    indices.push(index);
                }
            }
            crate::diag_log::log(
                "catalog",
                format!("list {}: {} cloud games in common", list.key, indices.len()),
            );
            // A list as big as the whole library (e.g. "all games") adds nothing.
            if !indices.is_empty() && indices.len() < titles.len() {
                collections.push(Collection {
                    key: list.key,
                    title: list.title,
                    indices,
                });
            }
        }

        // Keep the user on the same tab while new tabs appear.
        let current_key = self.current_category_key();
        self.service.collections = collections;
        self.service.server_recent_title_ids = fetched.recent_title_ids;
        self.restore_category(current_key);
    }

    /// Remembers a game the user started, for the "recently played" tab.
    pub(crate) fn note_title_played(&mut self, title_id: &str) {
        let current_key = self.current_category_key();
        let recent = &mut self.settings.recent_titles;
        recent.retain(|id| id != title_id);
        recent.insert(0, title_id.to_owned());
        recent.truncate(RECENT_LIMIT);
        self.settings.save();
        self.restore_category(current_key);
    }

    /// "Recently played": this Vita's history first, then the server's, without duplicates.
    pub(crate) fn recent_title_indices(&self) -> Vec<usize> {
        let mut indices = Vec::new();
        for title_id in self
            .settings
            .recent_titles
            .iter()
            .chain(&self.service.server_recent_title_ids)
        {
            if indices.len() >= RECENT_LIMIT {
                break;
            }
            if let Some(index) = self
                .service
                .titles
                .iter()
                .position(|title| &title.id == title_id)
                && !indices.contains(&index)
            {
                indices.push(index);
            }
        }
        indices
    }

    pub(super) fn collections_loading(&self) -> bool {
        self.service.collections_job.is_some() && matches!(&self.state, AppState::TitleList { .. })
    }
}
