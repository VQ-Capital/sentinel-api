// ========== DOSYA: sentinel-api/src/main.rs ==========
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
        .context("CRITICAL: NATS omurgasına bağlanılamadı.")?;

    info!("🔗 Sentinel-API (Full-Duplex Multiplexer) NATS omurgasına bağlandı.");

    let shared_state = Arc::new(AppState { nats_client });

    let app = Router::new()
        .route("/ws/v1/pipeline", get(ws_handler))
        .layer(CorsLayer::permissive())
        .with_state(shared_state);

    let addr = "0.0.0.0:8080";
    let listener = TcpListener::bind(addr).await?;
    info!("🚀 VQ-API Gateway {} üzerinde aktif.", addr);

    axum::serve(listener, app).await?;

    Ok(())
}

async fn ws_handler(ws: WebSocketUpgrade, State(state): State<Arc<AppState>>) -> impl IntoResponse {
    ws.on_upgrade(|socket| handle_socket(socket, state))
}

async fn handle_socket(socket: WebSocket, state: Arc<AppState>) {
    info!("📱 Terminal bağlantısı kabul edildi (Çift Yönlü Zarf Modu)");

    let (mut ws_sender, mut ws_receiver) = socket.split();

    // TASK 1: NATS'tan oku -> Terminal'e gönder
    let nats_sub_client = state.nats_client.clone();
    let send_task = tokio::spawn(async move {
        let mut sub = match nats_sub_client.subscribe("*.>").await {
            Ok(s) => s,
            Err(e) => {
                error!("NATS Abonelik Hatası: {}", e);
                return;
            }
        };

        while let Some(msg) = sub.next().await {
            let mut bundle = StreamBundle::default();

            if msg.subject.contains("market.trade") {
                if let Ok(trade) = AggTrade::decode(msg.payload.clone()) {
                    bundle.message = Some(BundleMsg::Trade(trade));
                }
            } else if msg.subject.contains("execution.report") {
                if let Ok(report) = ExecutionReport::decode(msg.payload.clone()) {
                    bundle.message = Some(BundleMsg::Report(report));
                }
            }

            if bundle.message.is_some() {
                let mut buf = Vec::new();
                if bundle.encode(&mut buf).is_ok()
                    && ws_sender.send(Message::Binary(buf)).await.is_err()
                {
                    warn!("🔌 Terminal bağlantısı koptu (Sender)");
                    break;
                }
            }
        }
    });

    // TASK 2: Terminalden oku -> NATS'a gönder (Kill Switch vb. komutlar için)
    let nats_pub_client = state.nats_client.clone();
    let recv_task = tokio::spawn(async move {
        while let Some(Ok(Message::Binary(bin))) = ws_receiver.next().await {
            if let Ok(bundle) = StreamBundle::decode(bin.as_slice()) {
                if let Some(BundleMsg::Command(cmd)) = bundle.message {
                    let mut buf = Vec::new();
                    if cmd.encode(&mut buf).is_ok() {
                        let _ = nats_pub_client
                            .publish("control.command".to_string(), buf.into())
                            .await;
                        warn!("🛑 [API] Terminalden KONTROL KOMUTU alındı ve NATS'a iletildi!");
                    }
                }
            }
        }
    });

    // İki task'ten biri kapanana kadar bekle
    tokio::select! {
        _ = send_task => {},
        _ = recv_task => {},
    }
}
