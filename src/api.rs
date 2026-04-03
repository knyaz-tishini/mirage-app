//! API client for mirage2server.
//!
//! Because the main loop is synchronous (winit), networking runs in a background
//! thread with its own single-threaded Tokio runtime. Communication happens via
//! `std::sync::mpsc` channels — the UI sends `ApiRequest` messages and receives
//! `ApiResponse` messages without blocking.

use std::sync::mpsc;
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Request / Response types
// ---------------------------------------------------------------------------

/// Messages the UI sends to the API worker.
pub enum ApiRequest {
    /// Check server health.
    Health,
    /// Fetch VPS providers (public endpoint, no auth needed).
    FetchProviders,
    /// Store a bearer token; subsequent authenticated requests will use it.
    SetToken(String),
    /// Fetch the current user's nodes.
    FetchNodes,
    /// Fetch the current user's chains.
    FetchChains,
    /// Activate a chain.
    ActivateChain { chain_id: i64 },
    /// Deactivate a chain.
    DeactivateChain { chain_id: i64 },
}

/// Messages the API worker sends back to the UI.
pub enum ApiResponse {
    Health(bool),
    Providers { l1: Vec<Provider>, l2: Vec<Provider> },
    TokenSet,
    Nodes(Vec<Node>),
    Chains(Vec<Chain>),
    ChainActivated { chain_id: i64 },
    ChainDeactivated { chain_id: i64 },
    Error(String),
}

// ---------------------------------------------------------------------------
// Domain types (mirrored from mirage2server API)
// ---------------------------------------------------------------------------

/// A VPS provider entry returned by `/api/providers`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Provider {
    pub id: i64,
    pub slug: String,
    pub name: String,
    pub tier: String,
    pub url: String,
    pub price_display: String,
    pub price_rub: Option<i64>,
    pub price_usd: Option<i64>,
    pub currency: String,
}

/// A user-owned VPS node.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Node {
    pub id: i64,
    pub user_id: i64,
    pub name: String,
    pub ip: String,
    pub ssh_port: i64,
    pub role: String,
    pub status: String,
    pub deploy_status: String,
    pub provider_slug: Option<String>,
    pub region: Option<String>,
}

/// A user-owned proxy chain (L1 + L2 node pair).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Chain {
    pub id: i64,
    pub user_id: i64,
    pub name: String,
    pub l1_node_id: i64,
    pub l2_node_id: i64,
    pub status: String,
}

// ---------------------------------------------------------------------------
// ApiClient
// ---------------------------------------------------------------------------

/// Async HTTP client bridged to the sync UI via `mpsc` channels.
pub struct ApiClient {
    tx: mpsc::Sender<ApiRequest>,
    rx: mpsc::Receiver<ApiResponse>,
}

impl ApiClient {
    /// Create a new client connecting to `base_url` (e.g. `"http://localhost:19400"`).
    ///
    /// Spawns a background OS thread with a single-threaded Tokio runtime that
    /// processes requests sequentially.
    pub fn new(base_url: &str) -> Self {
        let (req_tx, req_rx) = mpsc::channel::<ApiRequest>();
        let (resp_tx, resp_rx) = mpsc::channel::<ApiResponse>();

        let base = base_url.to_string();

        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("tokio runtime for API client failed to start");

            rt.block_on(async move {
                let client = reqwest::Client::builder()
                    .timeout(std::time::Duration::from_secs(10))
                    .build()
                    .expect("reqwest client failed to build");

                let mut token: Option<String> = None;

                while let Ok(req) = req_rx.recv() {
                    // Update token before dispatching so SetToken is reflected immediately.
                    if let ApiRequest::SetToken(ref t) = req {
                        token = Some(t.clone());
                    }
                    let resp = handle_request(&client, &base, &token, &req).await;
                    // Ignore send error — UI may have exited.
                    let _ = resp_tx.send(resp);
                }
            });
        });

        Self { tx: req_tx, rx: resp_rx }
    }

    /// Send a request to the background worker (non-blocking).
    pub fn send(&self, req: ApiRequest) {
        // Ignore send error — worker may have exited on shutdown.
        let _ = self.tx.send(req);
    }

    /// Non-blocking: try to receive one response. Returns `None` if none pending.
    pub fn try_recv(&self) -> Option<ApiResponse> {
        self.rx.try_recv().ok()
    }
}

// ---------------------------------------------------------------------------
// Request handler (runs inside background Tokio task)
// ---------------------------------------------------------------------------

async fn handle_request(
    client: &reqwest::Client,
    base: &str,
    token: &Option<String>,
    req: &ApiRequest,
) -> ApiResponse {
    match req {
        ApiRequest::Health => {
            match client.get(format!("{base}/api/health")).send().await {
                Ok(r) if r.status().is_success() => ApiResponse::Health(true),
                _ => ApiResponse::Health(false),
            }
        }

        ApiRequest::FetchProviders => {
            match client.get(format!("{base}/api/providers")).send().await {
                Ok(r) => match r.json::<serde_json::Value>().await {
                    Ok(v) => {
                        let l1: Vec<Provider> = serde_json::from_value(
                            v.get("l1").cloned().unwrap_or_default(),
                        )
                        .unwrap_or_default();
                        let l2: Vec<Provider> = serde_json::from_value(
                            v.get("l2").cloned().unwrap_or_default(),
                        )
                        .unwrap_or_default();
                        ApiResponse::Providers { l1, l2 }
                    }
                    Err(e) => ApiResponse::Error(format!("providers parse error: {e}")),
                },
                Err(e) => ApiResponse::Error(format!("fetch providers: {e}")),
            }
        }

        ApiRequest::SetToken(_) => ApiResponse::TokenSet,

        ApiRequest::FetchNodes => {
            let Some(t) = token else {
                return ApiResponse::Error("no token set".into());
            };
            match client
                .get(format!("{base}/api/owner/nodes"))
                .bearer_auth(t)
                .send()
                .await
            {
                Ok(r) => match r.json::<Vec<Node>>().await {
                    Ok(nodes) => ApiResponse::Nodes(nodes),
                    Err(e) => ApiResponse::Error(format!("parse nodes: {e}")),
                },
                Err(e) => ApiResponse::Error(format!("fetch nodes: {e}")),
            }
        }

        ApiRequest::FetchChains => {
            let Some(t) = token else {
                return ApiResponse::Error("no token set".into());
            };
            match client
                .get(format!("{base}/api/owner/chains"))
                .bearer_auth(t)
                .send()
                .await
            {
                Ok(r) => match r.json::<Vec<Chain>>().await {
                    Ok(chains) => ApiResponse::Chains(chains),
                    Err(e) => ApiResponse::Error(format!("parse chains: {e}")),
                },
                Err(e) => ApiResponse::Error(format!("fetch chains: {e}")),
            }
        }

        ApiRequest::ActivateChain { chain_id } => {
            let Some(t) = token else {
                return ApiResponse::Error("no token set".into());
            };
            match client
                .post(format!("{base}/api/owner/chains/{chain_id}/activate"))
                .bearer_auth(t)
                .send()
                .await
            {
                Ok(r) if r.status().is_success() => {
                    ApiResponse::ChainActivated { chain_id: *chain_id }
                }
                Ok(r) => ApiResponse::Error(format!("activate chain {chain_id}: {}", r.status())),
                Err(e) => ApiResponse::Error(format!("activate chain {chain_id}: {e}")),
            }
        }

        ApiRequest::DeactivateChain { chain_id } => {
            let Some(t) = token else {
                return ApiResponse::Error("no token set".into());
            };
            match client
                .post(format!("{base}/api/owner/chains/{chain_id}/deactivate"))
                .bearer_auth(t)
                .send()
                .await
            {
                Ok(r) if r.status().is_success() => {
                    ApiResponse::ChainDeactivated { chain_id: *chain_id }
                }
                Ok(r) => {
                    ApiResponse::Error(format!("deactivate chain {chain_id}: {}", r.status()))
                }
                Err(e) => ApiResponse::Error(format!("deactivate chain {chain_id}: {e}")),
            }
        }
    }
}
