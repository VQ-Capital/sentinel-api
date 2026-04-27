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
    Json, Router,
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
    pub mod wallet {
        pub mod v1 {
            include!(concat!(env!("OUT_DIR"), "/sentinel.wallet.v1.rs"));
        }
    }
}

use sentinel::api::v1::{stream_bundle::Message as BundleMsg, StreamBundle};
use sentinel::execution::v1::ExecutionReport;

// Sistem sağlığını tutacak yapı
struct AppState {
    nats_client: Client,
    report_history: RwLock<Vec<ExecutionReport>>,
    broadcast_tx: broadcast::Sender<StreamBundle>,
    uptime_start: std::time::Instant,
}

// Şeffaflık (Diagnose) Rapor Formatı
#[derive(serde::Serialize)]
struct SystemHealthReport {
    status: String,
    uptime_seconds: u64,
    active_ui_connections: usize,
    cached_reports: usize,
    message: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    info!(
        "📡 Service: {} | Version: {}",
        env!("CARGO_PKG_NAME"),
        env!("CARGO_PKG_VERSION")
    );

    let nats_url =
        std::env::var("NATS_URL").unwrap_or_else(|_| "nats://localhost:4222".to_string());
    let nats_client = async_nats::connect(&nats_url)
        .await
        .context("CRITICAL: NATS omurgasına bağlanılamadı.")?;

    info!("🔗 Sentinel-API (V3 Multiplexer) NATS omurgasına bağlandı.");

    let (tx, _rx) = broadcast::channel(2048);
    let shared_state = Arc::new(AppState {
        nats_client: nats_client.clone(),
        report_history: RwLock::new(Vec::new()),
        broadcast_tx: tx.clone(),
        uptime_start: std::time::Instant::now(),
    });

    // 🟢 GLOBAL NATS LISTENER (The Pipe)
    let state_clone = shared_state.clone();
    tokio::spawn(async move {
        let mut sub = state_clone.nats_client.subscribe("*.>").await.unwrap();

        while let Some(msg) = sub.next().await {
            let bundle_opt = if msg.subject.contains("market.trade") {
                if let Ok(trade) = sentinel::market::v1::AggTrade::decode(msg.payload.clone()) {
                    Some(StreamBundle {
                        message: Some(BundleMsg::Trade(trade)),
                    })
                } else {
                    None
                }
            } else if msg.subject.contains("execution.report") {
                if let Ok(report) =
                    sentinel::execution::v1::ExecutionReport::decode(msg.payload.clone())
                {
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
            } else if msg.subject.contains("wallet.equity") {
                if let Ok(equity) =
                    sentinel::wallet::v1::EquitySnapshot::decode(msg.payload.clone())
                {
                    Some(StreamBundle {
                        message: Some(BundleMsg::Equity(equity)),
                    })
                } else {
                    None
                }
            } else if msg.subject.contains("state.vector") {
                if let Ok(vector) =
                    sentinel::market::v1::MarketStateVector::decode(msg.payload.clone())
                {
                    Some(StreamBundle {
                        message: Some(BundleMsg::Vector(vector)),
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

    // 🚀 FAZ 2: HTTP REST Endpoints Eklendi
    let app = Router::new()
        .route("/ws/v1/pipeline", get(ws_handler))
        .route("/api/v1/diagnostics", get(diagnostics_handler)) // Yeni Diagnose Ucu
        .layer(CorsLayer::permissive())
        .with_state(shared_state);

    let addr = "0.0.0.0:8080";
    let listener = TcpListener::bind(addr).await?;
    info!("🚀 VQ-API Gateway is active on {}", addr);

    axum::serve(listener, app).await?;
    Ok(())
}

// 📊 Yeni REST Handler: UI bu JSON'u okuyup "System Health" penceresi çizebilir
async fn diagnostics_handler(State(state): State<Arc<AppState>>) -> Json<SystemHealthReport> {
    let history_len = state.report_history.read().await.len();
    let active_conns = state.broadcast_tx.receiver_count();
    let uptime = state.uptime_start.elapsed().as_secs();

    Json(SystemHealthReport {
        status: "OPERATIONAL".to_string(),
        uptime_seconds: uptime,
        active_ui_connections: active_conns,
        cached_reports: history_len,
        message: "Precision Engine Active. HFT streams normal.".to_string(),
    })
}

async fn ws_handler(ws: WebSocketUpgrade, State(state): State<Arc<AppState>>) -> impl IntoResponse {
    ws.on_upgrade(|socket| handle_socket(socket, state))
}

async fn handle_socket(socket: WebSocket, state: Arc<AppState>) {
    let (mut ws_sender, mut ws_receiver) = socket.split();
    let mut rx = state.broadcast_tx.subscribe();

    info!("📱 Yeni Terminal Bağlandı. Geçmiş veriler aktarılıyor...");

    // 1. Önbellekteki Raporları Gönder
    {
        let history = state.report_history.read().await;
        for report in history.iter() {
            let bundle = StreamBundle {
                message: Some(BundleMsg::Report(report.clone())),
            };
            let mut buf = Vec::new();
            if bundle.encode(&mut buf).is_ok() {
                let _ = ws_sender.send(Message::Binary(buf)).await;
            }
        }
    }

    // 2. Canlı Akışı Başlat
    let send_task = tokio::spawn(async move {
        while let Ok(bundle) = rx.recv().await {
            let mut buf = Vec::new();
            if bundle.encode(&mut buf).is_ok()
                && ws_sender.send(Message::Binary(buf)).await.is_err()
            {
                break;
            }
        }
    });

    // 3. Terminalden Gelen Komutlar (Kill Switch)
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
                        warn!("🛑 [API] KONTROL KOMUTU ALINDI VE SISTEME ILETILDI!");
                    }
                }
            }
        }
    });

    tokio::select! {
        _ = send_task => {},
        _ = recv_task => {},
    }
    info!("🔌 Terminal bağlantısı kesildi.");
}
