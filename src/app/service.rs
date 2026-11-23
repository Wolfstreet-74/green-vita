use super::image::{self, TitleImage};
use crate::api::catalog::worker::CatalogWorker;
use crate::api::catalog::{Game, GameCatalogBackend};
use crate::{ApiClient, ApiClientConfig, Console, MsalAuth};
use std::collections::HashSet;
use std::sync::Arc;

pub(crate) struct Service {
    pub(crate) api: ApiClient,
    pub(super) auth: MsalAuth,
    pub(crate) titles: Vec<Game>,
    pub(crate) consoles: Vec<Console>,
    pub(crate) catalog_worker: CatalogWorker,
    pub(crate) catalog_backend: GameCatalogBackend,
    pub(crate) title_detail_pending: HashSet<String>,
    pub(crate) box_art_pending: HashSet<String>,
    pub(crate) background_pending: HashSet<String>,
    pub(crate) thumb_pending: HashSet<String>,
    /// Set after a failed catalog request: no new metadata batch before this time.
    pub(crate) metadata_retry_after: Option<std::time::Instant>,
    pub(crate) avatar: Option<Arc<TitleImage>>,
    pub(crate) gamertag: Option<String>,
    pub(crate) gamerscore: Option<String>,
    pub(crate) logo: Arc<TitleImage>,
    /// Highlighted title and when it was highlighted, so large art is only loaded once the
    /// user stops scrolling.
    pub(crate) highlighted_since: Option<(usize, std::time::Instant)>,
    pub(crate) last_memory_log: Option<std::time::Instant>,
    /// Game Pass lists shown as grid tabs (popular, new, leaving soon).
    pub(crate) collections: Vec<super::collections::Collection>,
    /// xCloud's own "recently played" title ids, newest first.
    pub(crate) server_recent_title_ids: Vec<String>,
    pub(crate) collections_job:
        Option<tokio::task::JoinHandle<anyhow::Result<super::collections::FetchedCollections>>>,
}

impl Service {
    pub(super) fn new(locale: &str) -> Self {
        let mut api = ApiClient::new(ApiClientConfig::default());
        api.config.locale = locale.to_owned();

        let catalog_backend = GameCatalogBackend::xcloud(api.clone());
        let catalog_worker = CatalogWorker::spawn(catalog_backend.clone());
        Self {
            api,
            auth: MsalAuth::new(),
            titles: Vec::new(),
            consoles: Vec::new(),
            catalog_worker,
            catalog_backend,
            title_detail_pending: HashSet::new(),
            box_art_pending: HashSet::new(),
            background_pending: HashSet::new(),
            thumb_pending: HashSet::new(),
            metadata_retry_after: None,
            avatar: None,
            gamertag: None,
            gamerscore: None,
            logo: image::load_bundled_logo(),
            highlighted_since: None,
            last_memory_log: None,
            collections: Vec::new(),
            server_recent_title_ids: Vec::new(),
            collections_job: None,
        }
    }

    pub(super) fn logout(&mut self) {
        self.auth.logout();
        self.titles.clear();
        self.consoles.clear();
        self.collections.clear();
        self.server_recent_title_ids.clear();
        self.collections_job = None;
        self.avatar = None;
        self.gamertag = None;
        self.gamerscore = None;
        self.restart_catalog_worker();
    }

    pub(super) fn title_name_or_id(&self, title_id: &str) -> String {
        self.titles
            .iter()
            .find(|title| title.id == title_id)
            .map(|title| title.display_name().to_owned())
            .unwrap_or_else(|| title_id.to_owned())
    }

    pub(super) fn restart_catalog_worker(&mut self) {
        self.catalog_worker = CatalogWorker::spawn(self.catalog_backend.clone());
        self.title_detail_pending.clear();
        self.box_art_pending.clear();
        self.background_pending.clear();
        self.thumb_pending.clear();
        self.metadata_retry_after = None;
    }

    pub(super) fn refresh_xcloud_catalog_backend(&mut self) {
        self.catalog_backend = GameCatalogBackend::xcloud(self.api.clone());
        self.restart_catalog_worker();
    }
}
