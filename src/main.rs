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

struct AppState {
    nats_client: Client,
    report_history: RwLock<Vec<ExecutionReport>>,
    broadcast_tx: broadcast::Sender<StreamBundle>,
    uptime_start: std::time::Instant,
}

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
        "📡 Service: {} | Version: 1.0.0 (V7 RCA Support)",
        env!("CARGO_PKG_NAME")
    );

    let nats_url =
        std::env::var("NATS_URL").unwrap_or_else(|_| "nats://localhost:4222".to_string());
    let nats_client = async_nats::connect(&nats_url).await.context("NATS ERROR")?;

    let (tx, _rx) = broadcast::channel(2048);
    let shared_state = Arc::new(AppState {
        nats_client: nats_client.clone(),
        report_history: RwLock::new(Vec::new()),
        broadcast_tx: tx.clone(),
        uptime_start: std::time::Instant::now(),
    });

    let state_clone = shared_state.clone();
    tokio::spawn(async move {
        let mut sub = state_clone.nats_client.subscribe("*.>").await.unwrap();

        while let Some(msg) = sub.next().await {
            let bundle_opt = if msg.subject.contains("market.trade") {
                sentinel::market::v1::AggTrade::decode(msg.payload.clone())
                    .ok()
                    .map(|t| StreamBundle {
                        message: Some(BundleMsg::Trade(t)),
                    })
            } else if msg.subject.contains("execution.report") {
                sentinel::execution::v1::ExecutionReport::decode(msg.payload.clone())
                    .ok()
                    .map(|r| {
                        // Update cache for UI
                        let mut lock = state_clone.report_history.try_write();
                        if let Ok(ref mut history) = lock {
                            history.push(r.clone());
                            if history.len() > 100 {
                                history.remove(0);
                            }
                        }
                        StreamBundle {
                            message: Some(BundleMsg::Report(r)),
                        }
                    })
            } else if msg.subject.contains("wallet.equity") {
                sentinel::wallet::v1::EquitySnapshot::decode(msg.payload.clone())
                    .ok()
                    .map(|e| StreamBundle {
                        message: Some(BundleMsg::Equity(e)),
                    })
            } else if msg.subject.contains("state.vector") {
                sentinel::market::v1::MarketStateVector::decode(msg.payload.clone())
                    .ok()
                    .map(|v| StreamBundle {
                        message: Some(BundleMsg::Vector(v)),
                    })
            } else if msg.subject.contains("execution.rejection") {
                // 🔥 YENİ RCA HATTI
                sentinel::execution::v1::ExecutionRejection::decode(msg.payload.clone())
                    .ok()
                    .map(|rej| StreamBundle {
                        message: Some(BundleMsg::Rejection(rej)),
                    })
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
        .route("/api/v1/diagnostics", get(diagnostics_handler))
        .layer(CorsLayer::permissive())
        .with_state(shared_state);
    let listener = TcpListener::bind("0.0.0.0:8080").await?;
    axum::serve(listener, app).await?;
    Ok(())
}

async fn diagnostics_handler(State(state): State<Arc<AppState>>) -> Json<SystemHealthReport> {
    Json(SystemHealthReport {
        status: "OPERATIONAL".to_string(),
        uptime_seconds: state.uptime_start.elapsed().as_secs(),
        active_ui_connections: state.broadcast_tx.receiver_count(),
        cached_reports: state.report_history.read().await.len(),
        message: "V7 Precision Engine Active. RCA streams active.".to_string(),
    })
}

async fn ws_handler(ws: WebSocketUpgrade, State(state): State<Arc<AppState>>) -> impl IntoResponse {
    ws.on_upgrade(|socket| handle_socket(socket, state))
}

async fn handle_socket(socket: WebSocket, state: Arc<AppState>) {
    let (mut ws_sender, mut ws_receiver) = socket.split();
    let mut rx = state.broadcast_tx.subscribe();

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
                        warn!("🛑 [API] COMMAND RELAYED: {:?}", cmd.r#type);
                    }
                }
            }
        }
    });

    tokio::select! { _ = send_task => {}, _ = recv_task => {}, }
}
