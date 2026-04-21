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
use tokio::sync::{broadcast, RwLock};
use tower_http::cors::CorsLayer;
use tracing::{info, warn};

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
    report_history: RwLock<Vec<ExecutionReport>>,
    broadcast_tx: broadcast::Sender<StreamBundle>,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let nats_url =
        std::env::var("NATS_URL").unwrap_or_else(|_| "nats://localhost:4222".to_string());
    let nats_client = async_nats::connect(&nats_url)
        .await
        .context("CRITICAL: NATS omurgasına bağlanılamadı.")?;

    info!("🔗 Sentinel-API (Cached Multiplexer) NATS omurgasına bağlandı.");

    let (tx, _rx) = broadcast::channel(1024);
    let shared_state = Arc::new(AppState {
        nats_client: nats_client.clone(),
        report_history: RwLock::new(Vec::new()),
        broadcast_tx: tx.clone(),
    });

    // 🟢 GLOBAL NATS DİNLEYİCİSİ
    let state_clone = shared_state.clone();
    tokio::spawn(async move {
        let mut sub = state_clone.nats_client.subscribe("*.>").await.unwrap();
        while let Some(msg) = sub.next().await {
            // Clippy Uyumlu Doğrudan Nesne Yaratımı (Direct Initialization)
            let bundle_opt = if msg.subject.contains("market.trade") {
                if let Ok(trade) = AggTrade::decode(msg.payload.clone()) {
                    Some(StreamBundle {
                        message: Some(BundleMsg::Trade(trade)),
                    })
                } else {
                    None
                }
            } else if msg.subject.contains("execution.report") {
                if let Ok(report) = ExecutionReport::decode(msg.payload.clone()) {
                    let mut history = state_clone.report_history.write().await;
                    history.push(report.clone());
                    if history.len() > 100 {
                        history.remove(0);
                    }
                    Some(StreamBundle {
                        message: Some(BundleMsg::Report(report)),
                    })
                } else {
                    None
                }
            } else {
                None
            };

            if let Some(bundle) = bundle_opt {
                let _ = state_clone.broadcast_tx.send(bundle);
            }
        }
    });

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
    info!("📱 Yeni Terminal Bağlandı. Geçmiş Yükleniyor...");

    let (mut ws_sender, mut ws_receiver) = socket.split();
    let mut rx = state.broadcast_tx.subscribe();

    // 1. ADIM: EKRAN BOŞ KALMASIN DİYE ÖNCE GEÇMİŞ İŞLEMLERİ (CACHE) GÖNDER
    {
        let history = state.report_history.read().await;
        for report in history.iter() {
            // Clippy Uyumlu (Direct Initialization)
            let bundle = StreamBundle {
                message: Some(BundleMsg::Report(report.clone())),
            };
            let mut buf = Vec::new();
            if bundle.encode(&mut buf).is_ok() {
                let _ = ws_sender.send(Message::Binary(buf)).await;
            }
        }
    }
    info!("✅ Geçmiş başarıyla aktarıldı, Canlı Akışa geçiliyor.");

    // 2. ADIM: CANLI YAYINI TERMİNALE İLET
    let send_task = tokio::spawn(async move {
        while let Ok(bundle) = rx.recv().await {
            let mut buf = Vec::new();
            if bundle.encode(&mut buf).is_ok()
                && ws_sender.send(Message::Binary(buf)).await.is_err()
            {
                warn!("🔌 Terminal koptu, yayın kesildi.");
                break;
            }
        }
    });

    // 3. ADIM: TERMİNALDEN GELEN KOMUTLARI DİNLE
    let nats_pub = state.nats_client.clone();
    let recv_task = tokio::spawn(async move {
        while let Some(Ok(Message::Binary(bin))) = ws_receiver.next().await {
            if let Ok(bundle) = StreamBundle::decode(bin.as_slice()) {
                if let Some(BundleMsg::Command(cmd)) = bundle.message {
                    let mut buf = Vec::new();
                    if cmd.encode(&mut buf).is_ok() {
                        let _ = nats_pub
                            .publish("control.command".to_string(), buf.into())
                            .await;
                        warn!("🛑 [API] KONTROL KOMUTU ALINDI VE SİSTEME İLETİLDİ!");
                    }
                }
            }
        }
    });

    tokio::select! {
        _ = send_task => {},
        _ = recv_task => {},
    }
}
