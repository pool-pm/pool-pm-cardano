use axum::{
    http::StatusCode,
    response::sse::{Event as SseEvent, KeepAlive, Sse},
    routing::get,
    Router,
};
use futures::stream::Stream;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::RwLock;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::StreamExt;
use tower_http::cors::CorsLayer;
use tracing::info;

use crate::event_bus::EventBus;
use crate::filter;
use crate::state::State;

#[derive(Clone)]
struct AppState {
    bus: Arc<EventBus>,
    chain_state: Arc<RwLock<State>>,
    nftcdn_subdomain: &'static str,
    genesis: GenesisConfig,
}

#[derive(Clone, serde::Serialize)]
pub struct GenesisConfig {
    pub shelley_known_slot: u64,
    pub shelley_known_time: u64,
    pub shelley_slot_length: u32,
    pub byron_epoch_length: u32,
    pub shelley_epoch_length: u32,
}

fn serialize_event(event: crate::event::Event) -> Option<Result<SseEvent, Infallible>> {
    serde_json::to_string(&event)
        .ok()
        .map(|json| Ok(SseEvent::default().data(json)))
}

fn config_event(nftcdn: &str, genesis: &GenesisConfig) -> Result<SseEvent, Infallible> {
    let genesis_json = serde_json::to_string(genesis).unwrap();
    Ok(SseEvent::default().data(format!(
        "{{\"type\":\"Config\",\"nftcdn\":\"{}\",\"genesis\":{}}}",
        nftcdn, genesis_json
    )))
}

async fn events(
    axum::extract::State(state): axum::extract::State<AppState>,
) -> Sse<impl Stream<Item = Result<SseEvent, Infallible>>> {
    let (snapshot, rx) = state.bus.subscribe().await;

    let config = Some(config_event(state.nftcdn_subdomain, &state.genesis));

    let init = if snapshot.is_empty() {
        None
    } else {
        serde_json::to_string(&snapshot)
            .ok()
            .map(|json| Ok(SseEvent::default().data(json)))
    };
    let replay = futures::stream::iter(config.into_iter().chain(init));
    let live = BroadcastStream::new(rx).filter_map(|result| result.ok().and_then(serialize_event));
    let stream = replay.chain(live);

    Sse::new(stream).keep_alive(KeepAlive::default())
}

async fn filtered_events(
    axum::extract::State(state): axum::extract::State<AppState>,
    axum::extract::Path(feed_id): axum::extract::Path<String>,
) -> Result<Sse<impl Stream<Item = Result<SseEvent, Infallible>>>, StatusCode> {
    let filter = filter::FeedFilter::from_path(&feed_id).ok_or(StatusCode::BAD_REQUEST)?;

    let (snapshot, rx) = state.bus.subscribe().await;

    let delegators = {
        let guard = state.chain_state.read().await;
        guard
            .current()
            .and_then(|snap| filter.delegators(snap))
            .cloned()
            .unwrap_or_default()
    };

    let filtered_snapshot: Vec<crate::event::Event> = snapshot
        .into_iter()
        .filter_map(|e| filter.filter_event(&e, &delegators))
        .collect();

    let config = Some(config_event(state.nftcdn_subdomain, &state.genesis));

    let init = if filtered_snapshot.is_empty() {
        None
    } else {
        serde_json::to_string(&filtered_snapshot)
            .ok()
            .map(|json| Ok(SseEvent::default().data(json)))
    };
    let replay = futures::stream::iter(config.into_iter().chain(init));

    let chain_state = state.chain_state.clone();
    let live = futures::stream::unfold(
        (BroadcastStream::new(rx), filter, chain_state),
        |(mut rx, filter, chain_state)| async move {
            loop {
                let event = rx.next().await?.ok()?;
                let delegators = {
                    let guard = chain_state.read().await;
                    guard
                        .current()
                        .and_then(|snap| filter.delegators(snap))
                        .cloned()
                        .unwrap_or_default()
                };
                if let Some(sse) = filter
                    .filter_event(&event, &delegators)
                    .and_then(serialize_event)
                {
                    return Some((sse, (rx, filter, chain_state)));
                }
            }
        },
    );

    let stream = replay.chain(live);
    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

pub async fn serve(
    addr: SocketAddr,
    bus: Arc<EventBus>,
    chain_state: Arc<RwLock<State>>,
    nftcdn_subdomain: &'static str,
    genesis: GenesisConfig,
) {
    let state = AppState {
        bus,
        chain_state,
        nftcdn_subdomain,
        genesis,
    };
    let app = Router::new()
        .route("/events", get(events))
        .route("/events/{feed_id}", get(filtered_events))
        .layer(CorsLayer::permissive())
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    info!(%addr, "starting SSE server");
    axum::serve(listener, app).await.unwrap();
}
