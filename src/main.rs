#![deny(unsafe_code)]

use std::collections::HashMap;
use std::fmt::Display;
use std::str::FromStr;
use std::sync::Arc;

use actix_web::web::Data;
use actix_web::{web, App, HttpResponse, HttpServer};
use anyhow::Context;
use chrono::{DateTime, Utc};
use futures::StreamExt;
use parking_lot::RwLock;
use steam_api_concurrent::api::{Friend, PlayerBan, PlayerSummary};
use steam_api_concurrent::constants::{
    PLAYER_BANS_IDS_PER_REQUEST, PLAYER_SUMMARIES_IDS_PER_REQUEST,
};
use steam_api_concurrent::{SteamId, SteamIdStr};
use thiserror::Error;
use tokio::sync::{Semaphore, SemaphorePermit};
use tokio::task::JoinHandle;

#[derive(serde::Serialize)]
struct ResponseJson {
    steam_id: SteamIdStr,
    bans: HashMap<SteamId, PlayerBan>,
    friends: Option<HashMap<SteamId, Friend>>,
    summaries: HashMap<SteamId, PlayerSummary>,
}

struct ResponseJsonCached {
    inner: Arc<ResponseJson>,
    time: DateTime<Utc>,
}

#[derive(Error, Debug)]
enum Error {
    #[error("Failed to acquire a semaphore permit")]
    SemaphoreClosed,
    #[error("Failed to resolve vanity URL '{0}'")]
    InvalidVanityUrl(String),
    #[error("Failed to fetch friends list for {0}")]
    FriendsListRequest(SteamId),
    #[error("Failed to fetch bans for [{0}, ...]")]
    BansRequest(SteamId),
    #[error("Failed to fetch summary for [{0}, ...]")]
    SummaryRequest(SteamId),
    #[error("Failed to wait for future")]
    FutureClosed,
}

impl Error {
    pub fn into_response(self) -> HttpResponse {
        let body = self.to_string();
        match self {
            Error::SemaphoreClosed
            | Error::FriendsListRequest(_)
            | Error::BansRequest(_)
            | Error::SummaryRequest(_)
            | Error::FutureClosed => HttpResponse::InternalServerError().body(body),
            Error::InvalidVanityUrl(_) => HttpResponse::BadRequest().body(body),
        }
    }
}

struct ResponseJsonCache {
    cache_duration_ms: i64,
    inner: RwLock<HashMap<SteamId, ResponseJsonCached>>,
}

impl ResponseJsonCache {
    pub fn new(cache_duration_ms: i64) -> Self {
        ResponseJsonCache {
            cache_duration_ms,
            inner: Default::default(),
        }
    }
    pub fn get(&self, steam_id: SteamId) -> Option<Arc<ResponseJson>> {
        if self.cache_duration_ms <= 0 {
            return None;
        }

        self.inner
            .read()
            .get(&steam_id)
            .map(|cached| Arc::clone(&cached.inner))
    }
    pub fn set(&self, steam_id: SteamId, response_json: &Arc<ResponseJson>) {
        if self.cache_duration_ms <= 0 {
            return;
        }

        let mut lock = self.inner.write();

        // remove all expired entries
        let now = Utc::now();
        lock.retain(|_, cached| {
            let elapsed_ms = now.signed_duration_since(cached.time).num_milliseconds();
            elapsed_ms <= self.cache_duration_ms
        });

        // insert the new entry
        let _ = lock.insert(
            steam_id,
            ResponseJsonCached {
                inner: Arc::clone(response_json),
                time: Utc::now(),
            },
        );
    }
}

struct AppState {
    concurrent_requests: usize,
    api: steam_api_concurrent::Client,
    api_sem: Semaphore,
    cache: ResponseJsonCache,
}

impl AppState {
    #[tracing::instrument(level = "debug", skip(self))]
    async fn acquire_permit(&self) -> Result<SemaphorePermit, Error> {
        self.api_sem
            .acquire()
            .await
            .map_err(|_| Error::SemaphoreClosed)
    }

    #[tracing::instrument(level = "debug", skip(self))]
    async fn resolve_vanity_url(&self, vanity_url: &str) -> Result<SteamId, Error> {
        let _ = self.acquire_permit().await?;
        match self.api.resolve_vanity_url(vanity_url).await {
            Ok(steam_id) => Ok(steam_id),
            Err(_) => Err(Error::InvalidVanityUrl(vanity_url.to_string())),
        }
    }

    #[tracing::instrument(level = "debug", skip(self))]
    async fn get_player_friends(
        &self,
        steam_id: SteamId,
    ) -> Result<Option<HashMap<SteamId, Friend>>, Error> {
        let _ = self.acquire_permit().await?;
        match self.api.get_player_friends(steam_id).await {
            Ok(val) => Ok(val.into_inner()),
            Err(_) => Err(Error::FriendsListRequest(steam_id)),
        }
    }

    #[tracing::instrument(level = "debug", skip(self, steam_ids))]
    async fn get_player_bans(
        &self,
        steam_ids: &[SteamId],
    ) -> Result<HashMap<SteamId, PlayerBan>, Error> {
        assert!(!steam_ids.is_empty());
        assert!(steam_ids.len() <= PLAYER_BANS_IDS_PER_REQUEST);

        let _ = self.acquire_permit().await?;
        match self.api.get_player_bans(steam_ids.into()).await {
            Ok(val) => Ok(val.into_inner()),
            Err(_) => Err(Error::BansRequest(steam_ids[0])),
        }
    }

    #[tracing::instrument(level = "debug", skip(self, steam_ids))]
    async fn get_player_summaries(
        &self,
        steam_ids: &[SteamId],
    ) -> Result<HashMap<SteamId, PlayerSummary>, Error> {
        assert!(!steam_ids.is_empty());
        assert!(steam_ids.len() <= PLAYER_SUMMARIES_IDS_PER_REQUEST);

        let _ = self.acquire_permit().await?;
        match self.api.get_player_summaries(steam_ids.into()).await {
            Ok(val) => Ok(val.into_inner()),
            Err(_) => Err(Error::SummaryRequest(steam_ids[0])),
        }
    }
}

#[tracing::instrument(level = "debug", skip(state))]
async fn fetch_json_response(
    state: Data<AppState>,
    steam_id: SteamId,
) -> Result<Arc<ResponseJson>, Error> {
    let friends = state.get_player_friends(steam_id).await?;

    let mut query_ids = Vec::with_capacity(friends.as_ref().map(HashMap::len).unwrap_or(0) + 1);
    query_ids.push(steam_id);

    if let Some(friends) = friends.as_ref() {
        query_ids.extend(friends.values().map(|friend| friend.steam_id.steam_id()))
    }
    let query_ids = Arc::<[SteamId]>::from(query_ids);

    async fn fetch_bans(
        state: Data<AppState>,
        query_ids: Arc<[SteamId]>,
    ) -> Result<HashMap<SteamId, PlayerBan>, Error> {
        let mut bans = HashMap::<SteamId, PlayerBan>::with_capacity(query_ids.len());
        let mut chunks = futures::stream::iter(query_ids.chunks(PLAYER_BANS_IDS_PER_REQUEST))
            .map(|chunk| state.get_player_bans(chunk))
            .buffer_unordered(state.concurrent_requests);

        while let Some(bans_chunk) = chunks.next().await {
            bans.extend(bans_chunk?);
        }

        Ok(bans)
    }

    async fn fetch_summaries(
        state: Data<AppState>,
        query_ids: Arc<[SteamId]>,
    ) -> Result<HashMap<SteamId, PlayerSummary>, Error> {
        let mut summaries = HashMap::<SteamId, PlayerSummary>::with_capacity(query_ids.len());
        let mut chunks = futures::stream::iter(query_ids.chunks(PLAYER_SUMMARIES_IDS_PER_REQUEST))
            .map(|chunk| state.get_player_summaries(chunk))
            .buffer_unordered(state.concurrent_requests);

        while let Some(summaries_chunk) = chunks.next().await {
            summaries.extend(summaries_chunk?);
        }

        Ok(summaries)
    }

    async fn flatten<T>(handle: JoinHandle<Result<T, Error>>) -> Result<T, Error> {
        match handle.await {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(response)) => Err(response),
            Err(_) => Err(Error::FutureClosed),
        }
    }

    let bans_handle = tokio::task::spawn_local(fetch_bans(state.clone(), Arc::clone(&query_ids)));
    let summaries_handle =
        tokio::task::spawn_local(fetch_summaries(state.clone(), Arc::clone(&query_ids)));

    // await both handles or return the error early
    let (bans, summaries) = tokio::try_join!(flatten(bans_handle), flatten(summaries_handle))?;

    Ok(Arc::new(ResponseJson {
        steam_id: steam_id.into(),
        bans,
        friends,
        summaries,
    }))
}

async fn json_response(state: Data<AppState>, input: web::Path<(SteamIdStr,)>) -> HttpResponse {
    let steam_id = input.0.steam_id();

    if let Some(cached) = state.cache.get(steam_id) {
        return HttpResponse::Ok().json(cached.as_ref());
    }

    let response_json = match fetch_json_response(state.clone(), steam_id).await {
        Ok(response_json) => response_json,
        Err(err) => return err.into_response(),
    };

    state.cache.set(steam_id, &response_json);

    HttpResponse::Ok().json(response_json.as_ref())
}

async fn resolve_vanity_url(state: Data<AppState>, input: web::Path<(String,)>) -> HttpResponse {
    let vanity_url = input.0.as_str();
    let result = match state.resolve_vanity_url(vanity_url).await {
        Ok(result) => result,
        Err(err) => return err.into_response(),
    };
    HttpResponse::Ok().json(SteamIdStr::from(result))
}

#[tracing::instrument(level = "debug", skip(validator))]
fn load_env_var<T, F>(key: &str, validator: F) -> anyhow::Result<T>
where
    T: FromStr + Display,
    T::Err: Send + Sync + std::error::Error + 'static,
    F: FnOnce(&T) -> anyhow::Result<()>,
{
    let val =
        dotenv::var(key).with_context(|| format!("couldn't load environment variable {}", key))?;
    let val = T::from_str(val.as_str())
        .with_context(|| format!("couldn't parse environmant variable {} (={})", key, val))?;
    if let Err(err) = validator(&val) {
        return Err(err.context(format!(
            "couldn't validate environment variable {} (={})",
            key, val
        )));
    }
    Ok(val)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env_logger::init();

    if dotenv::dotenv().is_err() {
        log::warn!("No `.env` file found")
    }

    let steam_api_key = load_env_var("STEAM_INFO_API_STEAM_API_KEY", |_: &String| Ok(()))?;

    let cache_duration_ms = load_env_var("STEAM_INFO_API_CACHE_DURATION_MS", |&val: &i64| {
        anyhow::ensure!(val >= 0, "must be greater or equal to zero");
        Ok(())
    })?;

    let concurrent_requests =
        load_env_var("STEAM_INFO_API_CONCURRENT_REQUESTS", |&val: &usize| {
            anyhow::ensure!(val != 0, "must not be equal to zero");
            anyhow::ensure!(val <= 100, "calm down there partner",);
            Ok(())
        })?;

    let port = load_env_var("STEAM_INFO_API_PORT", |&val: &u16| {
        anyhow::ensure!(val != 0, "must not be equal to zero");
        Ok(())
    })?;

    if cache_duration_ms > 0 {
        log::info!("caching generated responses for {}ms", cache_duration_ms);
    } else {
        log::info!("not caching generated responses");
    }
    log::info!(
        "allowing up to {} concurrent requests to the steam api",
        concurrent_requests
    );
    log::info!("listening on port {}", port);

    let client = steam_api_concurrent::Client::builder()
        .api_key(steam_api_key)
        .dont_retry(401.try_into().unwrap())
        .build()
        .await?;

    let app_state = Data::new(AppState {
        concurrent_requests,
        api: client,
        api_sem: Semaphore::new(concurrent_requests),
        cache: ResponseJsonCache::new(cache_duration_ms),
    });

    HttpServer::new(move || {
        App::new()
            .app_data(app_state.clone())
            .route("/json/{steam_id}", web::get().to(json_response))
            .route("/vanity/{vanity}", web::get().to(resolve_vanity_url))
    })
    .bind(("0.0.0.0", port))?
    .run()
    .await?;

    Ok(())
}
