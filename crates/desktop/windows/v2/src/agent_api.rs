use std::sync::Arc;

use axum::{
    extract::State,
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use tokio::sync::{mpsc, Mutex};

use mirage_core::config::ClientConfig;

use crate::app::AgentCmd;
use crate::runtime::ProcessStats;

#[derive(serde::Serialize, serde::Deserialize, Clone)]
pub struct StatusResponse {
    pub state: String,
    pub stats: Option<ProcessStats>,
}

struct AgentApiState {
    cmd_tx: mpsc::Sender<AgentCmd>,
    status: Arc<Mutex<StatusResponse>>,
    logs: Arc<Mutex<Vec<String>>>,
}

pub fn spawn_agent_api(
    rt: &tokio::runtime::Handle,
    cmd_tx: mpsc::Sender<AgentCmd>,
    status: Arc<Mutex<StatusResponse>>,
    logs: Arc<Mutex<Vec<String>>>,
) {
    let state = Arc::new(AgentApiState { cmd_tx, status, logs });

    rt.spawn(async move {
        let app = Router::new()
            .route("/config", post(post_config))
            .route("/connect", post(post_connect))
            .route("/disconnect", post(post_disconnect))
            .route("/status", get(get_status))
            .route("/logs", get(get_logs))
            .route("/client-logs", get(get_client_logs))
            .with_state(state);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:19450")
            .await
            .expect("agent API bind failed on 127.0.0.1:19450");

        tracing::info!("agent API listening on 127.0.0.1:19450");
        axum::serve(listener, app).await.ok();
    });
}

async fn post_config(
    State(state): State<Arc<AgentApiState>>,
    Json(config): Json<ClientConfig>,
) -> StatusCode {
    let _ = state.cmd_tx.send(AgentCmd::SetConfig(config)).await;
    StatusCode::OK
}

async fn post_connect(State(state): State<Arc<AgentApiState>>) -> StatusCode {
    let _ = state.cmd_tx.send(AgentCmd::Connect).await;
    StatusCode::OK
}

async fn post_disconnect(State(state): State<Arc<AgentApiState>>) -> StatusCode {
    let _ = state.cmd_tx.send(AgentCmd::Disconnect).await;
    StatusCode::OK
}

async fn get_status(
    State(state): State<Arc<AgentApiState>>,
) -> Json<StatusResponse> {
    Json(state.status.lock().await.clone())
}

async fn get_logs(
    State(state): State<Arc<AgentApiState>>,
) -> Json<Vec<String>> {
    Json(state.logs.lock().await.clone())
}

async fn get_client_logs(
    State(state): State<Arc<AgentApiState>>,
) -> Json<Vec<String>> {
    let logs = state.logs.lock().await;
    let client_logs: Vec<String> = logs.iter()
        .filter(|l| l.contains("[CLIENT]"))
        .cloned()
        .collect();
    Json(client_logs)
}
