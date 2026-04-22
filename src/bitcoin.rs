use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use sha2::Sha256;
use thiserror::Error;
use tokio::time::sleep;

use crate::{
    DEFAULT_BITCOIND_IMAGE, RetryPolicy,
    docker::{
        ContainerRole, ContainerSpec, DockerClient, DockerError, SpawnedContainer,
        managed_container_labels,
    },
};

/// Default RPC user configured for spawned Bitcoin Core nodes.
pub const DEFAULT_BITCOIN_RPC_USER: &str = "bitcoinrpc";
/// Default wallet name used for mining and funding operations.
pub const DEFAULT_BITCOIN_WALLET_NAME: &str = "spawn-lnd";
/// Number of blocks mined to mature the default wallet's coinbase outputs.
pub const DEFAULT_BITCOIN_WALLET_MATURITY_BLOCKS: u64 = 150;
/// Regtest RPC port exposed by Bitcoin Core inside the Docker container.
pub const BITCOIND_RPC_PORT: u16 = 18443;
/// Regtest P2P port exposed by Bitcoin Core inside the Docker container.
pub const BITCOIND_P2P_PORT: u16 = 18444;

type HmacSha256 = Hmac<Sha256>;

/// Configuration for one spawned Bitcoin Core regtest backend.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct BitcoinCoreConfig {
    /// Cluster identifier used in container names and labels.
    pub cluster_id: String,
    /// Zero-based chain group index.
    pub group_index: usize,
    /// Docker image used for this Bitcoin Core container.
    pub image: String,
    /// Retry policy used while waiting for RPC readiness.
    pub startup_retry: RetryPolicy,
}

impl BitcoinCoreConfig {
    /// Create a Bitcoin Core config using the default pinned image.
    pub fn new(cluster_id: impl Into<String>, group_index: usize) -> Self {
        Self {
            cluster_id: cluster_id.into(),
            group_index,
            image: DEFAULT_BITCOIND_IMAGE.to_string(),
            startup_retry: RetryPolicy::default(),
        }
    }

    /// Override the Bitcoin Core Docker image.
    pub fn image(mut self, image: impl Into<String>) -> Self {
        self.image = image.into();
        self
    }

    /// Override the readiness retry policy.
    pub fn startup_retry_policy(mut self, policy: RetryPolicy) -> Self {
        self.startup_retry = policy;
        self
    }
}

/// A running Bitcoin Core container and its RPC handles.
#[derive(Clone, Debug)]
pub struct BitcoinCore {
    /// Docker container metadata.
    pub container: SpawnedContainer,
    /// RPC authentication generated for the node.
    pub auth: BitcoinRpcAuth,
    /// RPC client for node-level methods.
    pub rpc: BitcoinRpcClient,
    /// RPC client scoped to the default wallet.
    pub wallet_rpc: BitcoinRpcClient,
    /// Host RPC socket, usually `127.0.0.1:<port>`.
    pub rpc_socket: String,
    /// Host P2P socket, usually `127.0.0.1:<port>`.
    pub p2p_socket: String,
}

impl BitcoinCore {
    /// Spawn a Bitcoin Core container and wait until RPC is ready.
    pub async fn spawn(
        docker: &DockerClient,
        config: BitcoinCoreConfig,
    ) -> Result<Self, BitcoinCoreError> {
        let auth = BitcoinRpcAuth::random();
        let spec = bitcoind_container_spec(&config, &auth);
        let container = docker.create_and_start(spec).await?;
        let container_id = container.id.clone();
        let core = match Self::from_container(container, auth) {
            Ok(core) => core,
            Err(error) => {
                let logs = docker.container_logs(&container_id).await.ok();
                let _ = docker.rollback_containers([container_id.clone()]).await;
                return Err(BitcoinCoreError::Startup {
                    container_id,
                    logs,
                    source: Box::new(error),
                });
            }
        };

        if let Err(source) = core.wait_ready_with_policy(&config.startup_retry).await {
            let logs = docker.container_logs(&core.container.id).await.ok();
            let container_id = core.container.id.clone();
            let _ = docker.rollback_containers([container_id.clone()]).await;
            return Err(BitcoinCoreError::Startup {
                container_id,
                logs,
                source: Box::new(source),
            });
        }

        Ok(core)
    }

    fn from_container(
        container: SpawnedContainer,
        auth: BitcoinRpcAuth,
    ) -> Result<Self, BitcoinCoreError> {
        let rpc_port = container.host_port(BITCOIND_RPC_PORT).ok_or_else(|| {
            BitcoinCoreError::MissingHostPort {
                container_id: container.id.clone(),
                container_port: BITCOIND_RPC_PORT,
            }
        })?;
        let p2p_port = container.host_port(BITCOIND_P2P_PORT).ok_or_else(|| {
            BitcoinCoreError::MissingHostPort {
                container_id: container.id.clone(),
                container_port: BITCOIND_P2P_PORT,
            }
        })?;
        let rpc = BitcoinRpcClient::new("127.0.0.1", rpc_port, &auth.user, &auth.password);
        let wallet_rpc = rpc.wallet(DEFAULT_BITCOIN_WALLET_NAME);

        Ok(Self {
            rpc_socket: format!("127.0.0.1:{rpc_port}"),
            p2p_socket: format!("127.0.0.1:{p2p_port}"),
            container,
            auth,
            rpc,
            wallet_rpc,
        })
    }

    /// Wait for `getblockchaininfo` to succeed using the default retry policy.
    pub async fn wait_ready(&self) -> Result<BlockchainInfo, BitcoinCoreError> {
        self.wait_ready_with_policy(&RetryPolicy::default()).await
    }

    async fn wait_ready_with_policy(
        &self,
        policy: &RetryPolicy,
    ) -> Result<BlockchainInfo, BitcoinCoreError> {
        let mut last_error = None;

        for _ in 0..policy.attempts {
            match self.rpc.get_blockchain_info().await {
                Ok(info) => return Ok(info),
                Err(error) => {
                    last_error = Some(error);
                    sleep(policy.interval()).await;
                }
            }
        }

        Err(BitcoinCoreError::ReadyTimeout {
            attempts: policy.attempts,
            last_error: last_error.map(|error| error.to_string()),
        })
    }

    /// Create/load the default wallet and mine enough blocks to mature coinbase funds.
    pub async fn prepare_mining_wallet(&self) -> Result<Vec<String>, BitcoinCoreError> {
        self.rpc
            .ensure_wallet(DEFAULT_BITCOIN_WALLET_NAME)
            .await
            .map_err(BitcoinCoreError::BitcoinRpc)?;
        let address = self
            .wallet_rpc
            .get_new_address()
            .await
            .map_err(BitcoinCoreError::BitcoinRpc)?;

        self.rpc
            .generate_to_address(DEFAULT_BITCOIN_WALLET_MATURITY_BLOCKS, &address)
            .await
            .map_err(BitcoinCoreError::BitcoinRpc)
    }
}

/// RPC credentials for Bitcoin Core.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct BitcoinRpcAuth {
    /// RPC username.
    pub user: String,
    /// RPC password.
    pub password: String,
    /// Value suitable for Bitcoin Core's `-rpcauth` setting.
    pub rpcauth: String,
}

impl BitcoinRpcAuth {
    /// Generate random credentials with [`DEFAULT_BITCOIN_RPC_USER`].
    pub fn random() -> Self {
        Self::random_with_user(DEFAULT_BITCOIN_RPC_USER)
    }

    /// Generate random credentials for the given RPC user.
    pub fn random_with_user(user: impl Into<String>) -> Self {
        let user = user.into();
        let password = random_password();
        let salt = hex::encode(rand::random::<[u8; 16]>());
        let rpcauth = bitcoin_core_rpcauth(&user, &password, &salt);

        Self {
            user,
            password,
            rpcauth,
        }
    }
}

/// Minimal async JSON-RPC client for Bitcoin Core regtest nodes.
#[derive(Clone, Debug)]
pub struct BitcoinRpcClient {
    endpoint: String,
    user: String,
    password: String,
    client: reqwest::Client,
}

impl BitcoinRpcClient {
    /// Create a client for a host, port, and RPC credentials.
    pub fn new(
        host: impl AsRef<str>,
        port: u16,
        user: impl Into<String>,
        password: impl Into<String>,
    ) -> Self {
        Self {
            endpoint: format!("http://{}:{port}/", host.as_ref()),
            user: user.into(),
            password: password.into(),
            client: reqwest::Client::new(),
        }
    }

    /// Return the HTTP endpoint used by this client.
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// Return a client scoped to a named Bitcoin Core wallet.
    pub fn wallet(&self, wallet_name: &str) -> Self {
        Self {
            endpoint: format!(
                "{}/wallet/{wallet_name}",
                self.endpoint.trim_end_matches('/')
            ),
            user: self.user.clone(),
            password: self.password.clone(),
            client: self.client.clone(),
        }
    }

    /// Call `getblockchaininfo`.
    pub async fn get_blockchain_info(&self) -> Result<BlockchainInfo, BitcoinRpcError> {
        self.call("getblockchaininfo", json!([])).await
    }

    /// Call `listwallets`.
    pub async fn list_wallets(&self) -> Result<Vec<String>, BitcoinRpcError> {
        self.call("listwallets", json!([])).await
    }

    /// Call `createwallet`.
    pub async fn create_wallet(&self, wallet_name: &str) -> Result<CreateWallet, BitcoinRpcError> {
        self.call("createwallet", json!([wallet_name])).await
    }

    /// Call `loadwallet`.
    pub async fn load_wallet(&self, wallet_name: &str) -> Result<LoadWallet, BitcoinRpcError> {
        self.call("loadwallet", json!([wallet_name])).await
    }

    /// Ensure a wallet is loaded, creating it if it does not already exist.
    pub async fn ensure_wallet(&self, wallet_name: &str) -> Result<(), BitcoinRpcError> {
        if self
            .list_wallets()
            .await?
            .iter()
            .any(|loaded| loaded == wallet_name)
        {
            return Ok(());
        }

        match self.load_wallet(wallet_name).await {
            Ok(_) => Ok(()),
            Err(BitcoinRpcError::Rpc { .. }) => {
                self.create_wallet(wallet_name).await?;
                Ok(())
            }
            Err(error) => Err(error),
        }
    }

    /// Call `getnewaddress`.
    pub async fn get_new_address(&self) -> Result<String, BitcoinRpcError> {
        self.call("getnewaddress", json!([])).await
    }

    /// Mine `count` regtest blocks to `address`.
    pub async fn generate_to_address(
        &self,
        count: u64,
        address: &str,
    ) -> Result<Vec<String>, BitcoinRpcError> {
        self.call("generatetoaddress", json!([count, address]))
            .await
    }

    /// Call `getblock` with verbosity `1`.
    pub async fn get_block(&self, hash: &str) -> Result<BlockInfo, BitcoinRpcError> {
        self.call("getblock", json!([hash, 1])).await
    }

    /// Call `addnode <socket> add`.
    pub async fn add_node(&self, socket: &str) -> Result<(), BitcoinRpcError> {
        self.call_value("addnode", json!([socket, "add"])).await?;
        Ok(())
    }

    /// Call `sendtoaddress` and return the transaction id.
    pub async fn send_to_address(
        &self,
        address: &str,
        amount_btc: f64,
    ) -> Result<String, BitcoinRpcError> {
        self.call("sendtoaddress", json!([address, amount_btc]))
            .await
    }

    /// Call `sendmany` and return the transaction id.
    pub async fn send_many(
        &self,
        amounts: &std::collections::HashMap<String, f64>,
    ) -> Result<String, BitcoinRpcError> {
        self.call("sendmany", json!(["", amounts])).await
    }

    /// Call a JSON-RPC method and deserialize the `result` field.
    pub async fn call<T>(&self, method: &str, params: Value) -> Result<T, BitcoinRpcError>
    where
        T: DeserializeOwned,
    {
        let response = self.call_value(method, params).await?;

        serde_json::from_value(response).map_err(|source| BitcoinRpcError::DecodeResult {
            method: method.to_string(),
            source,
        })
    }

    /// Call a JSON-RPC method and return the raw JSON `result` field.
    pub async fn call_value(&self, method: &str, params: Value) -> Result<Value, BitcoinRpcError> {
        let response = self
            .client
            .post(&self.endpoint)
            .basic_auth(&self.user, Some(&self.password))
            .json(&JsonRpcRequest {
                jsonrpc: "1.0",
                id: "spawn-lnd",
                method,
                params,
            })
            .send()
            .await
            .map_err(|source| BitcoinRpcError::Request {
                method: method.to_string(),
                source,
            })?;

        let status = response.status();
        let body = response
            .text()
            .await
            .map_err(|source| BitcoinRpcError::ReadBody {
                method: method.to_string(),
                source,
            })?;

        let response: JsonRpcResponse =
            serde_json::from_str(&body).map_err(|source| BitcoinRpcError::Decode {
                method: method.to_string(),
                body,
                source,
            })?;

        if let Some(error) = response.error {
            return Err(BitcoinRpcError::Rpc {
                method: method.to_string(),
                code: error.code,
                message: error.message,
            });
        }

        if !status.is_success() {
            return Err(BitcoinRpcError::HttpStatus {
                method: method.to_string(),
                status: status.as_u16(),
            });
        }

        Ok(response.result)
    }
}

/// Subset of Bitcoin Core `getblockchaininfo` used by this crate.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct BlockchainInfo {
    /// Chain name, expected to be `regtest`.
    pub chain: String,
    /// Current validated block height.
    pub blocks: u64,
    /// Current header height.
    pub headers: u64,
    /// Best block hash.
    pub bestblockhash: String,
}

/// Subset of Bitcoin Core `getblock` response used by this crate.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct BlockInfo {
    /// Block hash.
    pub hash: String,
    /// Confirmation count when known.
    pub confirmations: Option<u64>,
    /// Block height when known.
    pub height: Option<u64>,
    /// Transaction ids included in the block.
    pub tx: Vec<String>,
}

/// Response from Bitcoin Core `createwallet`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CreateWallet {
    /// Created wallet name.
    pub name: String,
    /// Optional warning text returned by Bitcoin Core.
    #[serde(default)]
    pub warning: String,
}

/// Response from Bitcoin Core `loadwallet`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct LoadWallet {
    /// Loaded wallet name.
    pub name: String,
    /// Optional warning text returned by Bitcoin Core.
    #[serde(default)]
    pub warning: String,
}

/// Error returned by the Bitcoin Core JSON-RPC client.
#[derive(Debug, Error)]
pub enum BitcoinRpcError {
    /// The HTTP request failed before a response was received.
    #[error("Bitcoin Core RPC request failed for method {method}")]
    Request {
        /// RPC method name.
        method: String,
        /// Underlying HTTP client error.
        source: reqwest::Error,
    },

    /// The response body could not be read.
    #[error("failed to read Bitcoin Core RPC response body for method {method}")]
    ReadBody {
        /// RPC method name.
        method: String,
        /// Underlying HTTP client error.
        source: reqwest::Error,
    },

    /// The response body was not a valid Bitcoin Core JSON-RPC response.
    #[error("failed to decode Bitcoin Core RPC response for method {method}: {body}")]
    Decode {
        /// RPC method name.
        method: String,
        /// Raw response body.
        body: String,
        /// JSON decoding error.
        source: serde_json::Error,
    },

    /// The JSON-RPC `result` field could not be decoded into the requested type.
    #[error("failed to decode Bitcoin Core RPC result for method {method}")]
    DecodeResult {
        /// RPC method name.
        method: String,
        /// JSON decoding error.
        source: serde_json::Error,
    },

    /// Bitcoin Core returned a non-success HTTP status.
    #[error("Bitcoin Core RPC method {method} returned HTTP status {status}")]
    HttpStatus {
        /// RPC method name.
        method: String,
        /// HTTP status code.
        status: u16,
    },

    /// Bitcoin Core returned a JSON-RPC error object.
    #[error("Bitcoin Core RPC method {method} failed with code {code}: {message}")]
    Rpc {
        /// RPC method name.
        method: String,
        /// JSON-RPC error code.
        code: i64,
        /// JSON-RPC error message.
        message: String,
    },
}

/// Error returned while spawning or preparing Bitcoin Core.
#[derive(Debug, Error)]
pub enum BitcoinCoreError {
    /// Docker operation failed.
    #[error(transparent)]
    Docker(#[from] DockerError),

    /// Docker did not publish an expected port.
    #[error("Docker container {container_id} did not publish expected port {container_port}")]
    MissingHostPort {
        /// Docker container id.
        container_id: String,
        /// Expected container port.
        container_port: u16,
    },

    /// Bitcoin Core RPC did not become ready before timeout.
    #[error(
        "Bitcoin Core did not become ready after {attempts} attempts; last error: {last_error:?}"
    )]
    ReadyTimeout {
        /// Number of readiness attempts.
        attempts: usize,
        /// Last RPC error seen while waiting.
        last_error: Option<String>,
    },

    /// Bitcoin Core RPC failed.
    #[error(transparent)]
    BitcoinRpc(#[from] BitcoinRpcError),

    /// Container startup failed; logs are included when available.
    #[error("Bitcoin Core startup failed for container {container_id}; logs: {logs:?}")]
    Startup {
        /// Docker container id.
        container_id: String,
        /// Tail of container logs when available.
        logs: Option<String>,
        /// Underlying startup failure.
        source: Box<BitcoinCoreError>,
    },
}

#[derive(Serialize)]
struct JsonRpcRequest<'a> {
    jsonrpc: &'a str,
    id: &'a str,
    method: &'a str,
    params: Value,
}

#[derive(Deserialize)]
struct JsonRpcResponse {
    result: Value,
    error: Option<JsonRpcErrorObject>,
}

#[derive(Deserialize)]
struct JsonRpcErrorObject {
    code: i64,
    message: String,
}

/// Build the value for Bitcoin Core's `-rpcauth` flag.
pub fn bitcoin_core_rpcauth(user: &str, password: &str, salt: &str) -> String {
    let hmac = bitcoin_core_auth_hmac(password, salt);
    format!("{user}:{salt}${hmac}")
}

/// Compute Bitcoin Core's HMAC-SHA256 `rpcauth` digest.
pub fn bitcoin_core_auth_hmac(password: &str, salt: &str) -> String {
    let mut mac = HmacSha256::new_from_slice(salt.as_bytes()).expect("HMAC accepts any key length");
    mac.update(password.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

fn random_password() -> String {
    URL_SAFE_NO_PAD.encode(rand::random::<[u8; 32]>())
}

fn bitcoind_container_spec(config: &BitcoinCoreConfig, auth: &BitcoinRpcAuth) -> ContainerSpec {
    let name = format!(
        "spawn-lnd-{}-bitcoind-{}",
        config.cluster_id, config.group_index
    );
    let labels = managed_container_labels(&config.cluster_id, ContainerRole::Bitcoind, None);

    ContainerSpec::new(name, config.image.clone())
        .cmd(bitcoind_args(auth))
        .labels(labels)
        .expose_ports([BITCOIND_RPC_PORT, BITCOIND_P2P_PORT])
}

fn bitcoind_args(auth: &BitcoinRpcAuth) -> Vec<String> {
    vec![
        "-regtest".to_string(),
        "-printtoconsole".to_string(),
        "-rpcbind=0.0.0.0".to_string(),
        "-rpcallowip=0.0.0.0/0".to_string(),
        "-fallbackfee=0.00001".to_string(),
        "-server".to_string(),
        "-txindex".to_string(),
        "-blockfilterindex".to_string(),
        "-coinstatsindex".to_string(),
        format!("-rpcuser={}", auth.user),
        format!("-rpcpassword={}", auth.password),
    ]
}

#[cfg(test)]
mod tests {
    use super::{
        BITCOIND_P2P_PORT, BITCOIND_RPC_PORT, BitcoinCoreConfig, BitcoinRpcAuth, BitcoinRpcClient,
        DEFAULT_BITCOIN_RPC_USER, bitcoin_core_auth_hmac, bitcoin_core_rpcauth, bitcoind_args,
        bitcoind_container_spec,
    };
    use crate::DEFAULT_BITCOIND_IMAGE;

    #[test]
    fn derives_bitcoin_core_auth_hmac() {
        assert_eq!(
            bitcoin_core_auth_hmac("password", "salt"),
            "84ec44c7d6fc41917953a1dafca3c7d7856f7a9d0328b991b76f0d36be1224b9"
        );
    }

    #[test]
    fn derives_bitcoin_core_rpcauth() {
        assert_eq!(
            bitcoin_core_rpcauth("bitcoinrpc", "password", "salt"),
            "bitcoinrpc:salt$84ec44c7d6fc41917953a1dafca3c7d7856f7a9d0328b991b76f0d36be1224b9"
        );
    }

    #[test]
    fn random_auth_uses_default_user_and_rpcauth_shape() {
        let auth = BitcoinRpcAuth::random();
        let prefix = format!("{}:", DEFAULT_BITCOIN_RPC_USER);

        assert_eq!(auth.user, DEFAULT_BITCOIN_RPC_USER);
        assert!(!auth.password.is_empty());
        assert!(auth.rpcauth.starts_with(&prefix));
        assert!(auth.rpcauth.contains('$'));
    }

    #[test]
    fn builds_rpc_endpoint() {
        let client = BitcoinRpcClient::new("127.0.0.1", 18443, "user", "pass");

        assert_eq!(client.endpoint(), "http://127.0.0.1:18443/");
    }

    #[test]
    fn builds_wallet_rpc_endpoint() {
        let client = BitcoinRpcClient::new("127.0.0.1", 18443, "user", "pass");
        let wallet = client.wallet("spawn-lnd");

        assert_eq!(wallet.endpoint(), "http://127.0.0.1:18443/wallet/spawn-lnd");
    }

    #[test]
    fn default_bitcoin_core_config_uses_pinned_image() {
        let config = BitcoinCoreConfig::new("cluster-1", 2);

        assert_eq!(config.cluster_id, "cluster-1");
        assert_eq!(config.group_index, 2);
        assert_eq!(config.image, DEFAULT_BITCOIND_IMAGE);
    }

    #[test]
    fn builds_bitcoind_regtest_args() {
        let auth = BitcoinRpcAuth {
            user: "bitcoinrpc".to_string(),
            password: "password".to_string(),
            rpcauth: bitcoin_core_rpcauth("bitcoinrpc", "password", "salt"),
        };

        let args = bitcoind_args(&auth);

        assert!(args.contains(&"-regtest".to_string()));
        assert!(args.contains(&"-printtoconsole".to_string()));
        assert!(args.contains(&"-rpcbind=0.0.0.0".to_string()));
        assert!(args.contains(&"-rpcallowip=0.0.0.0/0".to_string()));
        assert!(args.contains(&"-server".to_string()));
        assert!(args.contains(&"-txindex".to_string()));
        assert!(args.contains(&"-fallbackfee=0.00001".to_string()));
        assert!(args.contains(&"-blockfilterindex".to_string()));
        assert!(args.contains(&"-coinstatsindex".to_string()));
        assert!(args.contains(&format!("-rpcuser={}", auth.user)));
        assert!(args.contains(&format!("-rpcpassword={}", auth.password)));
    }

    #[test]
    fn builds_bitcoind_container_spec() {
        let config = BitcoinCoreConfig::new("cluster-1", 0);
        let auth = BitcoinRpcAuth {
            user: "bitcoinrpc".to_string(),
            password: "password".to_string(),
            rpcauth: bitcoin_core_rpcauth("bitcoinrpc", "password", "salt"),
        };

        let spec = bitcoind_container_spec(&config, &auth);

        assert_eq!(spec.name, "spawn-lnd-cluster-1-bitcoind-0");
        assert_eq!(spec.image, DEFAULT_BITCOIND_IMAGE);
        assert!(spec.exposed_ports.contains(&BITCOIND_RPC_PORT));
        assert!(spec.exposed_ports.contains(&BITCOIND_P2P_PORT));
    }
}
