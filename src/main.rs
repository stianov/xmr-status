mod monero;

use std::{collections::BTreeSet, convert::Infallible, env, fs, net::SocketAddr, path::Path, sync::Arc, time::Duration};

use anyhow::{Context, Result};
use askama::Template;
use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    response::sse::{Event, KeepAlive, Sse},
    routing::get,
};
use serde::{Deserialize, Serialize};
use tokio::{
    sync::{RwLock, broadcast},
    time::{self, MissedTickBehavior},
};
use tokio_stream::{StreamExt, wrappers::BroadcastStream};

use crate::monero::{MoneroClient, MoneroError, NodeStatus, TxPoolSnapshot};
use tracing::{debug, error, info, warn};
use tracing_subscriber::EnvFilter;

#[derive(Clone)]
struct AppState {
    inner: Arc<AppStateInner>,
}

struct AppStateInner {
    monero_client: MoneroClient,
    status_cache: RwLock<Option<NodeStatus>>,
    status_tx: broadcast::Sender<NodeStatus>,
    txpool_cache: RwLock<TxPoolSnapshot>,
    txpool_tx: broadcast::Sender<TxPoolSnapshot>,
    config: Config,
}

const DEFAULT_POLL_INTERVAL_SECS: u64 = 1;
const CONFIG_FILE_PATH: &str = "config.toml";

const DEFAULT_ZMQ_TOPICS: &[&str] = &[
    "json-minimal-chain_main",
    "json-full-chain_main",
    "json-minimal-txpool_add",
    "json-full-txpool_add",
];

const VALID_ZMQ_TOPICS: &[&str] = &[
    "json-minimal-chain_main",
    "json-full-chain_main",
    "json-full-miner_data",
    "json-minimal-txpool_add",
    "json-full-txpool_add",
];

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Config {
    #[serde(rename = "description")]
    description: String,
    #[serde(rename = "title")]
    title: String,
    #[serde(rename = "serve-ip")]
    serve_ip: String,
    #[serde(rename = "serve-port")]
    serve_port: u16,
    #[serde(rename = "xmr-ip")]
    xmr_ip: String,
    #[serde(rename = "xmr-port")]
    xmr_port: u16,
    #[serde(rename = "xmr-zmq-ip", default = "Config::default_xmr_zmq_ip")]
    xmr_zmq_ip: String,
    #[serde(rename = "xmr-zmq-port", default = "Config::default_xmr_zmq_port")]
    xmr_zmq_port: u16,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            title: "MyExternalIp:MyExternalPort".to_string(),
            description: "Support the network, run a node!".to_string(),
            serve_ip: "0.0.0.0".to_string(),
            serve_port: 8080,
            xmr_ip: "127.0.0.1".to_string(),
            xmr_port: 18081,
            xmr_zmq_ip: "127.0.0.1".to_string(),
            xmr_zmq_port: 18083,
        }
    }
}

impl Config {
    fn load_or_create() -> Result<Self> {
        if Path::new(CONFIG_FILE_PATH).exists() {
            let content =
                fs::read_to_string(CONFIG_FILE_PATH).context("Failed to read config file")?;
            let config: Config = toml::from_str(&content).context("Failed to parse config file")?;
            debug!("Loaded configuration from {CONFIG_FILE_PATH}");
            Ok(config)
        } else {
            let config = Config::default();
            config.save()?;
            info!("Created default config file at {CONFIG_FILE_PATH}");
            Ok(config)
        }
    }

    fn save(&self) -> Result<()> {
        let content = toml::to_string_pretty(self).context("Failed to serialize config")?;
        fs::write(CONFIG_FILE_PATH, content).context("Failed to write config file")?;
        Ok(())
    }

    fn bind_address(&self) -> String {
        format!("{}:{}", self.serve_ip, self.serve_port)
    }

    fn monero_rpc_url(&self) -> String {
        format!("http://{}:{}", self.xmr_ip, self.xmr_port)
    }

    fn monero_zmq_url(&self) -> String {
        format!("tcp://{}:{}", self.xmr_zmq_ip, self.xmr_zmq_port)
    }

    fn default_xmr_zmq_ip() -> String {
        "127.0.0.1".to_string()
    }

    fn default_xmr_zmq_port() -> u16 {
        18083
    }
}

fn resolve_zmq_topics(raw_topics: Option<Vec<String>>) -> Vec<String> {
    let mut seen: BTreeSet<&'static str> = BTreeSet::new();

    let inputs = raw_topics.unwrap_or_else(|| {
        DEFAULT_ZMQ_TOPICS
            .iter()
            .map(|topic| (*topic).to_string())
            .collect::<Vec<_>>()
    });

    let mut resolved = Vec::new();

    for raw in inputs {
        match canonical_zmq_topic(&raw) {
            Ok(canonical) => {
                if seen.insert(canonical) {
                    resolved.push(canonical.to_string());
                }
            }
            Err(reason) => {
                warn!("Ignoring ZeroMQ topic `{}`: {}", raw, reason);
            }
        }
    }

    if resolved.is_empty() {
        warn!(
            "No valid ZeroMQ topics selected; falling back to defaults: {}",
            DEFAULT_ZMQ_TOPICS.join(", ")
        );

        for &topic in DEFAULT_ZMQ_TOPICS {
            if seen.insert(topic) {
                resolved.push(topic.to_string());
            }
        }
    }

    resolved
}

fn canonical_zmq_topic(raw: &str) -> Result<&'static str, &'static str> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("topic is empty");
    }

    if let Some(topic) = VALID_ZMQ_TOPICS
        .iter()
        .find(|candidate| candidate.eq_ignore_ascii_case(trimmed))
    {
        return Ok(*topic);
    }

    let normalized = trimmed
        .to_ascii_lowercase()
        .replace('_', "-")
        .replace('.', "-");

    match normalized.as_str() {
        "json-minimal-block" | "json-minimal-chain-main" => Ok("json-minimal-chain_main"),
        "json-full-block" | "json-full-chain-main" => Ok("json-full-chain_main"),
        "json-full-miner-data" => Ok("json-full-miner_data"),
        "json-minimal-txpool-add" => Ok("json-minimal-txpool_add"),
        "json-full-txpool-add" => Ok("json-full-txpool_add"),
        "json-minimal-txpool-remove" | "json-full-txpool-remove" => {
            Err("Monero no longer publishes txpool removal events over ZeroMQ")
        }
        _ => Err("unrecognized topic name"),
    }
}

type HandlerResult<T> = Result<T, (StatusCode, String)>;

fn init_tracing() {
    let env_filter = EnvFilter::try_from_default_env()
        .or_else(|_| EnvFilter::try_new("xmr_status=info"))
        .unwrap_or_else(|err| {
            eprintln!("Invalid RUST_LOG directive ({err}); falling back to xmr_status=info");
            EnvFilter::new("xmr_status=info")
        });

    if tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .with_target(false)
        .compact()
        .try_init()
        .is_err()
    {
        eprintln!(
            "Tracing subscriber already initialized; continuing with existing global subscriber"
        );
    }
}

#[derive(Template)]
#[template(path = "status.html")]
struct StatusTemplate {
    description: String,
    badge_label: String,
    badge_class: String,
    nettype: String,
    daemon_status: String,
    version: String,
    height: String,
    total_connections: String,
    status_json: String,
    txpool_json: String,
    title: String,
    show_details: bool,
}

impl AppState {
    fn new(monero_client: MoneroClient, config: Config) -> Self {
        let (status_tx, _) = broadcast::channel(32);
        let (txpool_tx, _) = broadcast::channel(32);
        Self {
            inner: Arc::new(AppStateInner {
                monero_client,
                status_cache: RwLock::new(None),
                status_tx,
                txpool_cache: RwLock::new(TxPoolSnapshot::empty()),
                txpool_tx,
                config,
            }),
        }
    }

    fn subscribe(&self) -> broadcast::Receiver<NodeStatus> {
        self.inner.status_tx.subscribe()
    }

    fn subscribe_txpool(&self) -> broadcast::Receiver<TxPoolSnapshot> {
        self.inner.txpool_tx.subscribe()
    }

    async fn cached_status(&self) -> Option<NodeStatus> {
        self.inner.status_cache.read().await.clone()
    }

    async fn cached_txpool(&self) -> TxPoolSnapshot {
        self.inner.txpool_cache.read().await.clone()
    }

    async fn ensure_status(&self) -> Result<NodeStatus, MoneroError> {
        if let Some(status) = self.cached_status().await {
            return Ok(status);
        }

        self.refresh_now().await
    }

    async fn refresh_now(&self) -> Result<NodeStatus, MoneroError> {
        let status = self.inner.monero_client.get_node_status().await?;
        self.store_status(status.clone()).await;

        match self.inner.monero_client.get_transaction_pool().await {
            Ok(entries) => self.store_txpool(entries).await,
            Err(err) => warn!("failed to refresh transaction pool: {err}"),
        }

        Ok(status)
    }

    async fn refresh_with_offline_fallback(&self) -> NodeStatus {
        match self.refresh_now().await {
            Ok(status) => status,
            Err(err) => self.mark_offline(format!("refresh failed: {err}")).await,
        }
    }

    async fn current_status(&self) -> NodeStatus {
        match self.ensure_status().await {
            Ok(status) => status,
            Err(err) => {
                self.mark_offline(format!("status request failed: {err}"))
                    .await
            }
        }
    }

    async fn store_status(&self, status: NodeStatus) {
        {
            let mut guard = self.inner.status_cache.write().await;
            *guard = Some(status.clone());
        }

        let _ = self.inner.status_tx.send(status);
    }

    async fn store_txpool(&self, snapshot: TxPoolSnapshot) {
        {
            let mut guard = self.inner.txpool_cache.write().await;
            *guard = snapshot.clone();
        }

        let _ = self.inner.txpool_tx.send(snapshot);
    }

    /// clear cached txpool data and mark as offline
    async fn mark_offline(&self, reason: impl Into<String>) -> NodeStatus {
        let reason = reason.into();
        warn!("marking node offline: {reason}");
        let offline_status = NodeStatus::offline();
        self.store_status(offline_status.clone()).await;
        self.store_txpool(TxPoolSnapshot::empty()).await;
        offline_status
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();

    let config = Config::load_or_create().context("Failed to load or create config file")?;

    let rpc_url = env::var("MONERO_RPC_URL").unwrap_or_else(|_| config.monero_rpc_url());
    let rpc_user = env::var("MONERO_RPC_USERNAME").ok();
    let rpc_pass = env::var("MONERO_RPC_PASSWORD").ok();
    let credentials = match (rpc_user, rpc_pass) {
        (Some(user), Some(pass)) => Some((user, pass)),
        _ => None,
    };

    let monero_client =
        MoneroClient::new(rpc_url, credentials).context("failed to build Monero RPC client")?;
    info!("Configured Monero RPC endpoint");

    let bind_addr = env::var("BIND_ADDRESS").unwrap_or_else(|_| config.bind_address());
    let addr: SocketAddr = bind_addr
        .parse()
        .with_context(|| format!("failed to parse bind address `{bind_addr}`"))?;
    info!("Binding HTTP server to {addr}");

    let poll_interval = env::var("MONERO_POLL_INTERVAL_SECS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or_else(|| Duration::from_secs(DEFAULT_POLL_INTERVAL_SECS));
    debug!("Using poll interval of {:?}", poll_interval);

    let state = AppState::new(monero_client, config.clone());
    let initial_status = state.refresh_with_offline_fallback().await;
    if initial_status.offline {
        warn!("Initial Monero status indicates the node is offline");
    } else {
        info!(
            "Initial Monero status fetched: height={}, incoming={}, outgoing={}",
            initial_status.height,
            initial_status.incoming_connections,
            initial_status.outgoing_connections
        );
    }

    spawn_polling_task(state.clone(), poll_interval);

    {
        let endpoint = env::var("MONERO_ZMQ_URL")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| config.monero_zmq_url());

        if !endpoint.trim().is_empty() {
            let raw_topics = env::var("MONERO_ZMQ_TOPICS")
                .ok()
                .map(|value| {
                    value
                        .split(',')
                        .filter_map(|topic| {
                            let trimmed = topic.trim();
                            if trimmed.is_empty() {
                                None
                            } else {
                                Some(trimmed.to_string())
                            }
                        })
                        .collect::<Vec<_>>()
                })
                .filter(|topics| !topics.is_empty());

            let topics = resolve_zmq_topics(raw_topics);

            info!(
                "Starting ZeroMQ listener on {endpoint} with topics: {}",
                topics.join(", ")
            );
            spawn_zmq_listener(state.clone(), endpoint, topics);
        } else {
            anyhow::bail!(
                "MONERO_ZMQ_URL or config.monero_zmq_url() must provide a non-empty endpoint"
            );
        }
    }

    use axum::routing::get_service;
    use tower_http::services::ServeDir;

    let app = Router::new()
        .route("/", get(render_status_page))
        .route("/api/status", get(status_json))
        .route("/events", get(status_events))
        .nest_service("/static", get_service(ServeDir::new("static")))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    let actual_addr = listener.local_addr()?;
    info!("🚀 Monero status server listening on http://{actual_addr}");

    axum::serve(listener, app).await?;

    Ok(())
}

async fn render_status_page(State(state): State<AppState>) -> HandlerResult<StatusTemplate> {
    let status = state.current_status().await;

    let txpool = if status.offline {
        TxPoolSnapshot::empty()
    } else {
        state.cached_txpool().await
    };

    let (badge_label, badge_class) = if status.offline {
        ("Offline", "status-badge offline")
    } else if status.synchronized {
        ("Synced", "status-badge online")
    } else {
        ("Syncing", "status-badge syncing")
    };

    let show_details = !status.offline;

    let daemon_status = if show_details {
        status.status_text.as_deref().unwrap_or("Unknown").to_string()
    } else {
        String::new()
    };

    let nettype = if show_details {
        status.nettype.as_deref().unwrap_or("unknown").to_string()
    } else {
        String::new()
    };

    let height = if show_details {
        format_number(status.height)
    } else {
        String::new()
    };

    let total_connections = if show_details {
        format_number((status.incoming_connections + status.outgoing_connections) as u64)
    } else {
        String::new()
    };

    let version = if show_details {
        status.version.clone().unwrap_or_else(|| "unknown".into())
    } else {
        String::new()
    };

    let status_json = serde_json::to_string(&status).unwrap_or_else(|_| "{}".into());
    let txpool_json = serde_json::to_string(&txpool)
        .or_else(|_| serde_json::to_string(&TxPoolSnapshot::empty()))
        .unwrap_or_else(|_| {
            "{\"entries\":[],\"totalTransactions\":0,\"totalWeightBytes\":0}".into()
        });

    let template = StatusTemplate {
        badge_label: badge_label.to_string(),
        badge_class: badge_class.to_string(),
        nettype,
        daemon_status,
        version,
        height,
        total_connections,
        status_json,
        txpool_json,
        title: state.inner.config.title.clone(),
        description: state.inner.config.description.clone(),
        show_details,
    };

    Ok(template)
}

async fn status_json(State(state): State<AppState>) -> HandlerResult<Json<NodeStatus>> {
    Ok(Json(state.current_status().await))
}

async fn status_events(
    State(state): State<AppState>,
) -> Sse<impl futures_core::stream::Stream<Item = Result<Event, Infallible>>> {
    // init cached data so the page renders without waiting for the first tick.
    let mut initial_events: Vec<Result<Event, Infallible>> = Vec::new();

    if let Some(status) = state.cached_status().await {
        if let Ok(payload) = serde_json::to_string(&status) {
            initial_events.push(Ok(Event::default().event("status").data(payload)));
        }
    }

    if let Ok(payload) = serde_json::to_string(&state.cached_txpool().await) {
        initial_events.push(Ok(Event::default().event("txpool").data(payload)));
    } else {
        warn!("failed to encode initial txpool for SSE");
    }

    let status_updates =
        BroadcastStream::new(state.subscribe()).filter_map(|result| match result {
            Ok(status) => match serde_json::to_string(&status) {
                Ok(payload) => Some(Ok(Event::default().event("status").data(payload))),
                Err(err) => {
                    warn!("failed to encode status for SSE: {err}");
                    None
                }
            },
            Err(err) => {
                warn!("status broadcast stream error: {err}");
                None
            }
        });

    let txpool_updates =
        BroadcastStream::new(state.subscribe_txpool()).filter_map(|result| match result {
            Ok(entries) => match serde_json::to_string(&entries) {
                Ok(payload) => Some(Ok(Event::default().event("txpool").data(payload))),
                Err(err) => {
                    warn!("failed to encode txpool for SSE: {err}");
                    None
                }
            },
            Err(err) => {
                warn!("txpool broadcast stream error: {err}");
                None
            }
        });

    let stream = tokio_stream::iter(initial_events).chain(status_updates.merge(txpool_updates));

    Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("keep-alive"),
    )
}

fn spawn_polling_task(state: AppState, interval: Duration) {
    tokio::spawn(async move {
        let interval = if interval < Duration::from_secs(1) {
            Duration::from_secs(1)
        } else {
            interval
        };

        let mut ticker = time::interval(interval);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
        debug!(
            "Started background polling loop with interval {:?}",
            ticker.period()
        );

        loop {
            ticker.tick().await;
            let status = state.refresh_with_offline_fallback().await;
            if status.offline {
                warn!("Polling detected node offline");
            } else {
                debug!("Polling refreshed status at height {}", status.height);
            }
        }
    });
}

fn spawn_zmq_listener(state: AppState, endpoint: String, topics: Vec<String>) {
    let handle = tokio::runtime::Handle::current();
    tokio::task::spawn_blocking(move || {
        if let Err(err) = run_zmq_listener(handle, state, endpoint, topics) {
            error!("ZeroMQ listener terminated: {err:?}");
        }
    });
}

fn run_zmq_listener(
    handle: tokio::runtime::Handle,
    state: AppState,
    endpoint: String,
    topics: Vec<String>,
) -> anyhow::Result<()> {
    use zmq::Context;

    let context = Context::new();
    let socket = context.socket(zmq::SUB)?;
    socket.connect(&endpoint)?;
    info!("ZeroMQ subscriber connected to {endpoint}");

    if topics.is_empty() {
        socket.set_subscribe(b"")?;
        debug!("Subscribed to all ZeroMQ topics");
    } else {
        for topic in topics {
            socket.set_subscribe(topic.as_bytes())?;
            debug!("Subscribed to ZeroMQ topic {topic}");
        }
    }

    loop {
        if let Err(err) = socket.recv_multipart(0) {
            warn!("ZeroMQ receive error: {err}");
            continue;
        }

        let state_clone = state.clone();
        debug!("ZeroMQ signaled new data; refreshing node status");
        if let Err(err) = handle.block_on(async {
            // Use the offline fallback method for ZMQ updates too
            let status = state_clone.refresh_with_offline_fallback().await;
            if status.offline {
                warn!("ZeroMQ-triggered refresh observed offline node");
            } else {
                debug!(
                    "ZeroMQ-triggered refresh succeeded at height {}",
                    status.height
                );
            }
            Ok::<(), MoneroError>(())
        }) {
            warn!("failed to refresh status after ZeroMQ signal: {err}");
        }
    }
}

fn format_number(value: u64) -> String {
    let mut s = value.to_string();
    let mut insert_pos = s.len() as isize - 3;
    while insert_pos > 0 {
        s.insert(insert_pos as usize, ' ');
        insert_pos -= 3;
    }
    s
}
