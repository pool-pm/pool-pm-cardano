use axum::{
    response::sse::{Event as SseEvent, KeepAlive, Sse},
    routing::get,
    Router,
};
use futures::stream::Stream;
use std::convert::Infallible;
use std::net::SocketAddr;
use tokio::sync::broadcast;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::StreamExt;
use tower_http::cors::CorsLayer;
use tracing::info;

use crate::event::Event;

async fn events(
    axum::extract::State(tx): axum::extract::State<broadcast::Sender<Event>>,
) -> Sse<impl Stream<Item = Result<SseEvent, Infallible>>> {
    let rx = tx.subscribe();
    let stream = BroadcastStream::new(rx).filter_map(|result| {
        result.ok().and_then(|event| {
            serde_json::to_string(&event)
                .ok()
                .map(|json| Ok(SseEvent::default().data(json)))
        })
    });
    Sse::new(stream).keep_alive(KeepAlive::default())
}

pub async fn serve(addr: SocketAddr, tx: broadcast::Sender<Event>) {
    let app = Router::new()
        .route("/events", get(events))
        .layer(CorsLayer::permissive())
        .with_state(tx);

    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    info!(%addr, "starting SSE server");
    axum::serve(listener, app).await.unwrap();
}
