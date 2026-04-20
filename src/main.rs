use anyhow::{Context, Result};
use async_nats::Client;
use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        State,
    },
    response::IntoResponse,
    routing::get,
    Router,
};
use futures_util::{SinkExt, StreamExt};
use std::sync::Arc;
use tokio::net::TcpListener;
use tower_http::cors::CorsLayer;
use tracing::{error, info, warn};

struct AppState {
    nats_client: Client,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let nats_url =
        std::env::var("NATS_URL").unwrap_or_else(|_| "nats://localhost:4222".to_string());
    let nats_client = async_nats::connect(&nats_url)
        .await
        .context("NATS Connection Failed")?;

    info!("🔗 Sentinel-API: NATS omurgasına binary modda bağlanıldı.");

    let shared_state = Arc::new(AppState { nats_client });

    let app = Router::new()
        // Tek bir endpoint üzerinden tüm stream'leri (Market + Execution) yöneteceğiz
        .route("/ws/v1/pipeline", get(ws_handler))
        .layer(CorsLayer::permissive())
        .with_state(shared_state);

    let addr = "0.0.0.0:8080";
    let listener = TcpListener::bind(addr).await?;
    info!("🚀 VQ-API Gateway (Binary) {} üzerinde aktif.", addr);

    axum::serve(listener, app).await?;
    Ok(())
}

async fn ws_handler(ws: WebSocketUpgrade, State(state): State<Arc<AppState>>) -> impl IntoResponse {
    ws.on_upgrade(|socket| handle_socket(socket, state))
}

async fn handle_socket(mut socket: WebSocket, state: Arc<AppState>) {
    info!("📱 Terminal bağlantısı kabul edildi (Protobuf Stream)");

    // UI'ın dinlemesi gereken tüm kanallar
    // market.trade.> | execution.report.> | signal.trade.>
    let mut sub = match state.nats_client.subscribe("*.>").await {
        Ok(s) => s,
        Err(e) => {
            error!("NATS Subscription hatası: {}", e);
            return;
        }
    };

    while let Some(msg) = sub.next().await {
        // KRİTİK: Veriyi JSON'a çevirmiyoruz!
        // Ham Protobuf byte dizisini (payload) doğrudan Binary Message olarak gönderiyoruz.
        if socket
            .send(Message::Binary(msg.payload.to_vec()))
            .await
            .is_err()
        {
            warn!("🔌 Terminal bağlantısı koptu.");
            break;
        }
    }
}
