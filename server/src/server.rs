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

fn serialize_event(event: crate::event::Event) -> Option<Result<SseEvent, Infallible>> {
    serde_json::to_string(&event)
        .ok()
        .map(|json| Ok(SseEvent::default().data(json)))
}

async fn events(
    axum::extract::State(bus): axum::extract::State<Arc<EventBus>>,
) -> Sse<impl Stream<Item = Result<SseEvent, Infallible>>> {
    let (snapshot, rx) = bus.subscribe().await;

    let replay = futures::stream::iter(snapshot.into_iter().filter_map(serialize_event));
    let live = BroadcastStream::new(rx).filter_map(|result| result.ok().and_then(serialize_event));
    let stream = replay.chain(live);

    Sse::new(stream).keep_alive(KeepAlive::default())
}

pub async fn serve(addr: SocketAddr, bus: Arc<EventBus>) {
    let app = Router::new()
        .route("/events", get(events))
        .layer(CorsLayer::permissive())
        .with_state(bus);

    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    info!(%addr, "starting SSE server");
    axum::serve(listener, app).await.unwrap();
}
