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

pub const DEFAULT_BITCOIN_RPC_USER: &str = "bitcoinrpc";
pub const DEFAULT_BITCOIN_WALLET_NAME: &str = "spawn-lnd";
pub const DEFAULT_BITCOIN_WALLET_MATURITY_BLOCKS: u64 = 150;
pub const BITCOIND_RPC_PORT: u16 = 18443;
pub const BITCOIND_P2P_PORT: u16 = 18444;

type HmacSha256 = Hmac<Sha256>;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct BitcoinCoreConfig {
    pub cluster_id: String,
    pub group_index: usize,
    pub image: String,
    pub startup_retry: RetryPolicy,
}

impl BitcoinCoreConfig {
    pub fn new(cluster_id: impl Into<String>, group_index: usize) -> Self {
        Self {
            cluster_id: cluster_id.into(),
            group_index,
            image: DEFAULT_BITCOIND_IMAGE.to_string(),
            startup_retry: RetryPolicy::default(),
        }
    }

    pub fn image(mut self, image: impl Into<String>) -> Self {
        self.image = image.into();
        self
    }

    pub fn startup_retry_policy(mut self, policy: RetryPolicy) -> Self {
        self.startup_retry = policy;
        self
    }
}

#[derive(Clone, Debug)]
pub struct BitcoinCore {
    pub container: SpawnedContainer,
    pub auth: BitcoinRpcAuth,
    pub rpc: BitcoinRpcClient,
    pub wallet_rpc: BitcoinRpcClient,
    pub rpc_socket: String,
    pub p2p_socket: String,
}

impl BitcoinCore {
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

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct BitcoinRpcAuth {
    pub user: String,
    pub password: String,
    pub rpcauth: String,
}

impl BitcoinRpcAuth {
    pub fn random() -> Self {
        Self::random_with_user(DEFAULT_BITCOIN_RPC_USER)
    }

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

#[derive(Clone, Debug)]
pub struct BitcoinRpcClient {
    endpoint: String,
    user: String,
    password: String,
    client: reqwest::Client,
}

impl BitcoinRpcClient {
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

    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

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

    pub async fn get_blockchain_info(&self) -> Result<BlockchainInfo, BitcoinRpcError> {
        self.call("getblockchaininfo", json!([])).await
    }

    pub async fn list_wallets(&self) -> Result<Vec<String>, BitcoinRpcError> {
        self.call("listwallets", json!([])).await
    }

    pub async fn create_wallet(&self, wallet_name: &str) -> Result<CreateWallet, BitcoinRpcError> {
        self.call("createwallet", json!([wallet_name])).await
    }

    pub async fn load_wallet(&self, wallet_name: &str) -> Result<LoadWallet, BitcoinRpcError> {
        self.call("loadwallet", json!([wallet_name])).await
    }

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

    pub async fn get_new_address(&self) -> Result<String, BitcoinRpcError> {
        self.call("getnewaddress", json!([])).await
    }

    pub async fn generate_to_address(
        &self,
        count: u64,
        address: &str,
    ) -> Result<Vec<String>, BitcoinRpcError> {
        self.call("generatetoaddress", json!([count, address]))
            .await
    }

    pub async fn get_block(&self, hash: &str) -> Result<BlockInfo, BitcoinRpcError> {
        self.call("getblock", json!([hash, 1])).await
    }

    pub async fn add_node(&self, socket: &str) -> Result<(), BitcoinRpcError> {
        self.call_value("addnode", json!([socket, "add"])).await?;
        Ok(())
    }

    pub async fn send_to_address(
        &self,
        address: &str,
        amount_btc: f64,
    ) -> Result<String, BitcoinRpcError> {
        self.call("sendtoaddress", json!([address, amount_btc]))
            .await
    }

    pub async fn send_many(
        &self,
        amounts: &std::collections::HashMap<String, f64>,
    ) -> Result<String, BitcoinRpcError> {
        self.call("sendmany", json!(["", amounts])).await
    }

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

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct BlockchainInfo {
    pub chain: String,
    pub blocks: u64,
    pub headers: u64,
    pub bestblockhash: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct BlockInfo {
    pub hash: String,
    pub confirmations: Option<u64>,
    pub height: Option<u64>,
    pub tx: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CreateWallet {
    pub name: String,
    #[serde(default)]
    pub warning: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct LoadWallet {
    pub name: String,
    #[serde(default)]
    pub warning: String,
}

#[derive(Debug, Error)]
pub enum BitcoinRpcError {
    #[error("Bitcoin Core RPC request failed for method {method}")]
    Request {
        method: String,
        source: reqwest::Error,
    },

    #[error("failed to read Bitcoin Core RPC response body for method {method}")]
    ReadBody {
        method: String,
        source: reqwest::Error,
    },

    #[error("failed to decode Bitcoin Core RPC response for method {method}: {body}")]
    Decode {
        method: String,
        body: String,
        source: serde_json::Error,
    },

    #[error("failed to decode Bitcoin Core RPC result for method {method}")]
    DecodeResult {
        method: String,
        source: serde_json::Error,
    },

    #[error("Bitcoin Core RPC method {method} returned HTTP status {status}")]
    HttpStatus { method: String, status: u16 },

    #[error("Bitcoin Core RPC method {method} failed with code {code}: {message}")]
    Rpc {
        method: String,
        code: i64,
        message: String,
    },
}

#[derive(Debug, Error)]
pub enum BitcoinCoreError {
    #[error(transparent)]
    Docker(#[from] DockerError),

    #[error("Docker container {container_id} did not publish expected port {container_port}")]
    MissingHostPort {
        container_id: String,
        container_port: u16,
    },

    #[error(
        "Bitcoin Core did not become ready after {attempts} attempts; last error: {last_error:?}"
    )]
    ReadyTimeout {
        attempts: usize,
        last_error: Option<String>,
    },

    #[error(transparent)]
    BitcoinRpc(#[from] BitcoinRpcError),

    #[error("Bitcoin Core startup failed for container {container_id}; logs: {logs:?}")]
    Startup {
        container_id: String,
        logs: Option<String>,
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

pub fn bitcoin_core_rpcauth(user: &str, password: &str, salt: &str) -> String {
    let hmac = bitcoin_core_auth_hmac(password, salt);
    format!("{user}:{salt}${hmac}")
}

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
