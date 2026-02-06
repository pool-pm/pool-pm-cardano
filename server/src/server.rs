use axum::{
    response::sse::{Event as SseEvent, KeepAlive, Sse},
    routing::get,
    Router,
};
use futures::stream::Stream;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::StreamExt;
use tower_http::cors::CorsLayer;
use tracing::info;

use crate::event_bus::EventBus;

#[derive(Clone)]
struct AppState {
    bus: Arc<EventBus>,
    nftcdn_subdomain: &'static str,
}

fn serialize_event(event: crate::event::Event) -> Option<Result<SseEvent, Infallible>> {
    serde_json::to_string(&event)
        .ok()
        .map(|json| Ok(SseEvent::default().data(json)))
}

async fn events(
    axum::extract::State(state): axum::extract::State<AppState>,
) -> Sse<impl Stream<Item = Result<SseEvent, Infallible>>> {
    let (snapshot, rx) = state.bus.subscribe().await;

    let config = Some(Ok(SseEvent::default()
        .data(format!("{{\"type\":\"Config\",\"nftcdn\":\"{}\"}}", state.nftcdn_subdomain))));

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

pub async fn serve(addr: SocketAddr, bus: Arc<EventBus>, nftcdn_subdomain: &'static str) {
    let state = AppState { bus, nftcdn_subdomain };
    let app = Router::new()
        .route("/events", get(events))
        .layer(CorsLayer::permissive())
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    info!(%addr, "starting SSE server");
    axum::serve(listener, app).await.unwrap();
}
