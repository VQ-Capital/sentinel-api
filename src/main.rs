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
use prost::Message as ProstMessage;
use std::sync::Arc;
use tokio::net::TcpListener;
use tower_http::cors::CorsLayer;
use tracing::{error, info, warn};

// --- PROTOBUF MODÜLLERİ ---
// sentinel-spec submodule'undan gelen şemaları dahil ediyoruz
pub mod sentinel {
    pub mod market {
        pub mod v1 {
            include!(concat!(env!("OUT_DIR"), "/sentinel.market.v1.rs"));
        }
    }
    pub mod execution {
        pub mod v1 {
            include!(concat!(env!("OUT_DIR"), "/sentinel.execution.v1.rs"));
        }
    }
    pub mod api {
        pub mod v1 {
            include!(concat!(env!("OUT_DIR"), "/sentinel.api.v1.rs"));
        }
    }
}

use sentinel::api::v1::{stream_bundle::Message as BundleMsg, StreamBundle};
use sentinel::execution::v1::ExecutionReport;
use sentinel::market::v1::AggTrade;

// Axum içinde paylaşacağımız durum
struct AppState {
    nats_client: Client,
}

#[tokio::main]
async fn main() -> Result<()> {
    // 1. Loglama Sistemini Başlat
    tracing_subscriber::fmt::init();

    // 2. NATS Bağlantısı
    let nats_url = std::env::var("NATS_URL").unwrap_or_else(|_| "nats://localhost:4222".to_string());
    let nats_client = async_nats::connect(&nats_url)
        .await
        .context("CRITICAL: NATS omurgasına bağlanılamadı.")?;

    info!("🔗 Sentinel-API (Binary Streamer) NATS omurgasına bağlandı.");

    let shared_state = Arc::new(AppState { nats_client });

    // 3. HTTP Rotaları ve CORS Ayarları
    let app = Router::new()
        .route("/ws/v1/pipeline", get(ws_handler))
        .layer(CorsLayer::permissive())
        .with_state(shared_state);

    // 4. Sunucuyu Başlat
    let addr = "0.0.0.0:8080";
    let listener = TcpListener::bind(addr).await?;
    info!("🚀 VQ-API Gateway (Binary Multiplexer) {} üzerinde aktif.", addr);

    axum::serve(listener, app).await?;

    Ok(())
}

// WebSocket isteğini kabul eden handler
async fn ws_handler(ws: WebSocketUpgrade, State(state): State<Arc<AppState>>) -> impl IntoResponse {
    ws.on_upgrade(|socket| handle_socket(socket, state))
}

// Asıl veri trafiğinin döndüğü fonksiyon
async fn handle_socket(mut socket: WebSocket, state: Arc<AppState>) {
    info!("📱 Terminal bağlantısı kabul edildi (Zarf Modu)");

    // NATS üzerindeki tüm pazar ve işlem kanallarını dinle (*.>)
    let mut sub = match state.nats_client.subscribe("*.>").await {
        Ok(s) => s,
        Err(e) => {
            error!("NATS Abonelik Hatası: {}", e);
            return;
        }
    };

    while let Some(msg) = sub.next().await {
        // Her mesaj için yeni bir "Zarf" (Bundle) oluştur
        let mut bundle = StreamBundle::default();

        // NATS Subject'ine göre veriyi decode et ve zarfa koy
        if msg.subject.contains("market.trade") {
            // Ham byte'ı AggTrade nesnesine çevir
            if let Ok(trade) = AggTrade::decode(msg.payload.clone()) {
                bundle.message = Some(BundleMsg::Trade(trade));
            }
        } else if msg.subject.contains("execution.report") {
            // Ham byte'ı ExecutionReport nesnesine çevir
            if let Ok(report) = ExecutionReport::decode(msg.payload.clone()) {
                bundle.message = Some(BundleMsg::Report(report));
            }
        }

        // Eğer zarfın içine geçerli bir mesaj koyabildiysek Flutter'a gönder
        if bundle.message.is_some() {
            let mut buf = Vec::new();
            if let Ok(_) = bundle.encode(&mut buf) {
                // Zarfı binary olarak WebSocket üzerinden gönder
                if let Err(e) = socket.send(Message::Binary(buf.into())).await {
                    warn!("🔌 Terminal bağlantısı koptu: {}", e);
                    break;
                }
            }
        }
    }
}