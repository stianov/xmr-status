use std::{cmp::Reverse, time::Duration};

use reqwest::Client;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thiserror::Error;
use tracing::{debug, trace, warn};

#[derive(Clone)]
pub struct MoneroClient {
    http: Client,
    endpoint: String,
    base_url: String,
    credentials: Option<Credentials>,
}

#[derive(Clone)]
struct Credentials {
    username: String,
    password: String,
}

impl MoneroClient {
    pub fn new(
        base_url: impl Into<String>,
        credentials: Option<(String, String)>,
    ) -> Result<Self, MoneroError> {
        let base_url = base_url.into();
        if base_url.trim().is_empty() {
            return Err(MoneroError::InvalidEndpoint);
        }

        let trimmed = base_url.trim_end_matches('/').to_string();
        let endpoint = format!("{}/json_rpc", trimmed);

        let http = Client::builder().timeout(Duration::from_secs(3)).build()?;

        let credentials =
            credentials.map(|(username, password)| Credentials { username, password });

        debug!("Initialized Monero RPC client for {trimmed}");

        Ok(Self {
            http,
            endpoint,
            base_url: trimmed,
            credentials,
        })
    }

    pub async fn get_node_status(&self) -> Result<NodeStatus, MoneroError> {
        let info: GetInfoResult = self.call_rpc("get_info", json!({})).await?;

        Ok(NodeStatus::from(info))
    }

    pub async fn get_transaction_pool(&self) -> Result<TxPoolSnapshot, MoneroError> {
        let response: GetTransactionPoolResponse = self
            .call_other_rpc("get_transaction_pool", json!({}))
            .await?;

        if let Some(status) = response.status.as_deref() {
            if !status.eq_ignore_ascii_case("ok") {
                return Err(MoneroError::Rpc {
                    code: 0,
                    message: format!("get_transaction_pool returned status {status}"),
                });
            }
        }

        Ok(Self::snapshot_from_transactions(response.transactions))
    }

    fn snapshot_from_transactions(transactions: Option<Vec<TransactionPoolTx>>) -> TxPoolSnapshot {
        let mut entries: Vec<TxPoolEntry> = transactions
            .unwrap_or_default()
            .into_iter()
            .filter_map(TxPoolEntry::from_rpc)
            .collect();

        entries.sort_by_key(|entry| Reverse(entry.receive_time.unwrap_or(0)));

        let total_transactions = entries.len() as u64;
        let total_weight_bytes = entries.iter().map(|entry| entry.weight.unwrap_or(0)).sum();

        entries.truncate(10);

        TxPoolSnapshot {
            entries,
            total_transactions,
            total_weight_bytes,
        }
    }

    async fn call_rpc<T>(&self, method: &str, params: Value) -> Result<T, MoneroError>
    where
        T: DeserializeOwned,
    {
        debug!("Calling Monero RPC method {method}");
        let payload = json!({
            "jsonrpc": "2.0",
            "id": "xmr-status",
            "method": method,
            "params": params,
        });
        trace!("RPC payload prepared: {payload}");

        let mut request = self.http.post(&self.endpoint).json(&payload);

        if let Some(creds) = &self.credentials {
            request = request.basic_auth(&creds.username, Some(&creds.password));
        }

        let response = request.send().await?.error_for_status()?;

        let rpc_response: RpcResponse<T> = response.json().await?;

        if let Some(error) = rpc_response.error {
            warn!(
                "Monero RPC method {method} returned error code {}: {}",
                error.code, error.message
            );
            return Err(MoneroError::Rpc {
                code: error.code,
                message: error.message,
            });
        }

        rpc_response.result.ok_or(MoneroError::MissingResult)
    }

    async fn call_other_rpc<T>(&self, path: &str, body: Value) -> Result<T, MoneroError>
    where
        T: DeserializeOwned,
    {
        debug!("Calling Monero RPC endpoint {path}");

        let path = path.trim_start_matches('/');
        let url = format!("{}/{}", self.base_url, path);

        let mut request = self.http.post(url).json(&body);

        if let Some(creds) = &self.credentials {
            request = request.basic_auth(&creds.username, Some(&creds.password));
        }

        let response = request.send().await?.error_for_status()?;

        Ok(response.json().await?)
    }
}

#[derive(Debug, Error)]
pub enum MoneroError {
    #[error("monero rpc endpoint is empty")]
    InvalidEndpoint,
    #[error("network error: {0}")]
    Network(#[from] reqwest::Error),
    #[error("json-rpc error {code}: {message}")]
    Rpc { code: i64, message: String },
    #[error("json-rpc response missing result payload")]
    MissingResult,
}

#[derive(Debug, Deserialize)]
struct RpcResponse<T> {
    result: Option<T>,
    error: Option<RpcError>,
}

#[derive(Debug, Deserialize)]
struct RpcError {
    code: i64,
    message: String,
}

#[derive(Debug, Deserialize)]
struct GetInfoResult {
    height: u64,
    target_height: Option<u64>,
    nettype: Option<String>,
    incoming_connections_count: Option<u32>,
    outgoing_connections_count: Option<u32>,
    tx_pool_size: Option<u32>,
    synchronized: Option<bool>,
    offline: Option<bool>,
    status: Option<String>,
    version: Option<VersionField>,
}

#[derive(Debug, Deserialize)]
struct GetTransactionPoolResponse {
    status: Option<String>,
    transactions: Option<Vec<TransactionPoolTx>>,
}

#[derive(Debug, Deserialize)]
struct TransactionPoolTx {
    tx_hash: Option<String>,
    id_hash: Option<String>,
    receive_time: Option<u64>,
    last_relayed_time: Option<u64>,
    fee: Option<u64>,
    weight: Option<u64>,
    blob_size: Option<u64>,
    relayed: Option<bool>,
    double_spend_seen: Option<bool>,
    kept_by_block: Option<bool>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NodeStatus {
    pub status_text: Option<String>,
    pub nettype: Option<String>,
    pub height: u64,
    pub target_height: Option<u64>,
    pub incoming_connections: u32,
    pub outgoing_connections: u32,
    pub tx_pool_size: u32,
    pub synchronized: bool,
    pub offline: bool,
    pub version: Option<String>,
    pub remaining_blocks: Option<i64>,
    pub sync_progress: Option<f64>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TxPoolEntry {
    pub hash: String,
    pub receive_time: Option<u64>,
    pub last_relayed_time: Option<u64>,
    pub fee: Option<u64>,
    pub weight: Option<u64>,
    pub relayed: Option<bool>,
    pub double_spend_seen: Option<bool>,
    pub kept_by_block: Option<bool>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TxPoolSnapshot {
    pub entries: Vec<TxPoolEntry>,
    pub total_transactions: u64,
    pub total_weight_bytes: u64,
}

impl TxPoolSnapshot {
    pub fn empty() -> Self {
        Self {
            entries: Vec::new(),
            total_transactions: 0,
            total_weight_bytes: 0,
        }
    }
}

impl TxPoolEntry {
    fn from_rpc(tx: TransactionPoolTx) -> Option<Self> {
        let hash = tx.tx_hash.or(tx.id_hash)?;

        Some(Self {
            hash,
            receive_time: tx.receive_time,
            last_relayed_time: tx.last_relayed_time,
            fee: tx.fee,
            weight: tx.weight.or(tx.blob_size),
            relayed: tx.relayed,
            double_spend_seen: tx.double_spend_seen,
            kept_by_block: tx.kept_by_block,
        })
    }
}

impl NodeStatus {
    /// filler data if offline, not displayed anywhere
    pub fn offline() -> Self {
        Self {
            status_text: Some("Offline".to_string()),
            nettype: Some("unknown".to_string()),
            height: 0,
            target_height: None,
            incoming_connections: 0,
            outgoing_connections: 0,
            tx_pool_size: 0,
            synchronized: false,
            offline: true,
            version: None,
            remaining_blocks: None,
            sync_progress: None,
        }
    }
}

impl From<GetInfoResult> for NodeStatus {
    fn from(info: GetInfoResult) -> Self {
        let target_height_for_calc = info.target_height.filter(|h| *h > 0).unwrap_or(info.height);
        let remaining_blocks = info
            .target_height
            .filter(|h| *h > 0)
            .map(|target| target as i64 - info.height as i64);
        let sync_progress = if target_height_for_calc == 0 {
            Some(1.0)
        } else {
            Some((info.height as f64 / target_height_for_calc as f64).clamp(0.0, 1.0))
        };

        let synchronized = info
            .synchronized
            .unwrap_or_else(|| sync_progress.map(|v| v >= 0.999).unwrap_or(true));

        NodeStatus {
            status_text: info.status,
            nettype: info.nettype,
            height: info.height,
            target_height: info.target_height,
            incoming_connections: info.incoming_connections_count.unwrap_or_default(),
            outgoing_connections: info.outgoing_connections_count.unwrap_or_default(),
            tx_pool_size: info.tx_pool_size.unwrap_or_default(),
            synchronized,
            offline: info.offline.unwrap_or(false),
            version: info.version.as_ref().map(VersionField::to_display_string),
            remaining_blocks,
            sync_progress,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum VersionField {
    Numeric(u32),
    Text(String),
}

impl VersionField {
    fn to_display_string(&self) -> String {
        match self {
            VersionField::Numeric(value) => {
                let major = value >> 16;
                let minor = (value >> 8) & 0xff;
                let patch = value & 0xff;
                format!("{major}.{minor}.{patch}")
            }
            VersionField::Text(text) => text.trim().to_string(),
        }
    }
}
