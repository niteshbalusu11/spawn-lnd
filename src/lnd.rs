use hyper::Uri;
use lnd_grpc_rust::{
    LndClient, LndNodeConfig, MyChannel,
    lnrpc::{
        GenSeedRequest, GetInfoRequest, GetInfoResponse, InitWalletRequest,
        wallet_unlocker_client::WalletUnlockerClient,
    },
};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::time::{Duration, sleep};

use crate::{
    BitcoinCore, DEFAULT_LND_IMAGE,
    bitcoin::BITCOIND_RPC_PORT,
    docker::{
        ContainerRole, ContainerSpec, DockerClient, DockerError, SpawnedContainer,
        managed_container_labels,
    },
};

pub const LND_GRPC_PORT: u16 = 10009;
pub const LND_P2P_PORT: u16 = 9735;
pub const LND_TLS_CERT_PATH: &str = "/root/.lnd/tls.cert";
pub const LND_ADMIN_MACAROON_PATH: &str = "/root/.lnd/data/chain/bitcoin/regtest/admin.macaroon";
pub const LND_WALLET_PASSWORD: &[u8] = b"password";
pub const DEFAULT_GENERATE_ADDRESS: &str = "2N8hwP1WmJrFF5QWABn38y63uYLhnJYJYTF";

const READY_RETRY_ATTEMPTS: usize = 500;
const READY_RETRY_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct LndConfig {
    pub cluster_id: String,
    pub alias: String,
    pub node_index: usize,
    pub image: String,
    pub extra_args: Vec<String>,
}

impl LndConfig {
    pub fn new(cluster_id: impl Into<String>, alias: impl Into<String>, node_index: usize) -> Self {
        Self {
            cluster_id: cluster_id.into(),
            alias: alias.into(),
            node_index,
            image: DEFAULT_LND_IMAGE.to_string(),
            extra_args: Vec::new(),
        }
    }

    pub fn image(mut self, image: impl Into<String>) -> Self {
        self.image = image.into();
        self
    }

    pub fn extra_arg(mut self, arg: impl Into<String>) -> Self {
        self.extra_args.push(arg.into());
        self
    }

    pub fn extra_args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.extra_args.extend(args.into_iter().map(Into::into));
        self
    }
}

#[derive(Clone, Debug)]
pub struct LndDaemon {
    pub alias: String,
    pub container: SpawnedContainer,
    pub cert_hex: String,
    pub macaroon_hex: String,
    pub rpc_socket: String,
    pub p2p_socket: String,
    pub public_key: String,
}

impl LndDaemon {
    pub async fn spawn(
        docker: &DockerClient,
        bitcoind: &BitcoinCore,
        config: LndConfig,
    ) -> Result<Self, LndError> {
        bitcoind
            .rpc
            .generate_to_address(1, DEFAULT_GENERATE_ADDRESS)
            .await
            .map_err(LndError::BitcoinRpc)?;

        let spec = lnd_container_spec(&config, bitcoind)?;
        let container = docker.create_and_start(spec).await?;
        let container_id = container.id.clone();
        let result = Self::initialize_started(docker, container, config.alias).await;

        match result {
            Ok(daemon) => Ok(daemon),
            Err(error) => {
                let _ = docker.rollback_containers([container_id]).await;
                Err(error)
            }
        }
    }

    async fn initialize_started(
        docker: &DockerClient,
        container: SpawnedContainer,
        alias: String,
    ) -> Result<Self, LndError> {
        let rpc_port =
            container
                .host_port(LND_GRPC_PORT)
                .ok_or_else(|| LndError::MissingHostPort {
                    container_id: container.id.clone(),
                    container_port: LND_GRPC_PORT,
                })?;
        let p2p_port =
            container
                .host_port(LND_P2P_PORT)
                .ok_or_else(|| LndError::MissingHostPort {
                    container_id: container.id.clone(),
                    container_port: LND_P2P_PORT,
                })?;
        let rpc_socket = format!("127.0.0.1:{rpc_port}");
        let cert_bytes = wait_for_file(docker, &container.id, LND_TLS_CERT_PATH).await?;
        let cert_hex = hex::encode(&cert_bytes);
        let macaroon_hex =
            init_wallet_or_read_macaroon(docker, &container.id, &cert_bytes, &rpc_socket).await?;
        let info = wait_for_synced_get_info(&cert_hex, &macaroon_hex, &rpc_socket).await?;

        Ok(Self {
            alias,
            p2p_socket: format!("127.0.0.1:{p2p_port}"),
            public_key: info.identity_pubkey,
            cert_hex,
            macaroon_hex,
            rpc_socket,
            container,
        })
    }

    pub fn node_config(&self) -> LndNodeConfig {
        LndNodeConfig::new(
            self.alias.clone(),
            self.cert_hex.clone(),
            self.macaroon_hex.clone(),
            self.rpc_socket.clone(),
        )
    }

    pub async fn connect(&self) -> Result<LndClient, LndError> {
        connect_authenticated(&self.cert_hex, &self.macaroon_hex, &self.rpc_socket).await
    }

    pub async fn wait_synced_to_chain(&self) -> Result<GetInfoResponse, LndError> {
        wait_for_synced_get_info(&self.cert_hex, &self.macaroon_hex, &self.rpc_socket).await
    }
}

#[derive(Debug, Error)]
pub enum LndError {
    #[error(transparent)]
    Docker(#[from] DockerError),

    #[error(transparent)]
    BitcoinRpc(#[from] crate::BitcoinRpcError),

    #[error("Bitcoin Core container did not expose a bridge IP address for LND")]
    MissingBitcoindIp,

    #[error("Docker container {container_id} did not publish expected LND port {container_port}")]
    MissingHostPort {
        container_id: String,
        container_port: u16,
    },

    #[error("failed to connect to LND at {socket}: {message}")]
    Connect { socket: String, message: String },

    #[error("failed to create unauthenticated LND channel to {socket}: {message}")]
    UnauthenticatedChannel { socket: String, message: String },

    #[error(
        "LND wallet init did not complete after {attempts} attempts; last error: {last_error:?}"
    )]
    WalletInitTimeout {
        attempts: usize,
        last_error: Option<String>,
    },

    #[error(
        "LND did not report synced_to_chain after {attempts} attempts; last error: {last_error:?}"
    )]
    ReadyTimeout {
        attempts: usize,
        last_error: Option<String>,
    },

    #[error(
        "LND {container_id} did not produce file {path} after {attempts} attempts; last error: {last_error:?}"
    )]
    FileTimeout {
        container_id: String,
        path: String,
        attempts: usize,
        last_error: Option<String>,
    },
}

fn lnd_container_spec(
    config: &LndConfig,
    bitcoind: &BitcoinCore,
) -> Result<ContainerSpec, LndError> {
    let bitcoind_ip = bitcoind
        .container
        .ip_address
        .as_deref()
        .ok_or(LndError::MissingBitcoindIp)?;
    let name = format!(
        "spawn-lnd-{}-lnd-{}-{}",
        config.cluster_id, config.node_index, config.alias
    );
    let labels =
        managed_container_labels(&config.cluster_id, ContainerRole::Lnd, Some(&config.alias));
    let mut args = lnd_args(bitcoind_ip, bitcoind);

    args.extend(config.extra_args.clone());

    Ok(ContainerSpec::new(name, config.image.clone())
        .cmd(args)
        .labels(labels)
        .expose_ports([LND_GRPC_PORT, LND_P2P_PORT]))
}

fn lnd_args(bitcoind_ip: &str, bitcoind: &BitcoinCore) -> Vec<String> {
    vec![
        "--bitcoin.regtest".to_string(),
        "--bitcoin.node=bitcoind".to_string(),
        "--bitcoind.rpcpolling".to_string(),
        format!("--bitcoind.rpchost={bitcoind_ip}:{BITCOIND_RPC_PORT}"),
        format!("--bitcoind.rpcuser={}", bitcoind.auth.user),
        format!("--bitcoind.rpcpass={}", bitcoind.auth.password),
        "--debuglevel=info".to_string(),
        "--noseedbackup".to_string(),
        "--listen=0.0.0.0:9735".to_string(),
        "--rpclisten=0.0.0.0:10009".to_string(),
    ]
}

async fn wait_for_file(
    docker: &DockerClient,
    container_id: &str,
    path: &str,
) -> Result<Vec<u8>, LndError> {
    let mut last_error = None;

    for _ in 0..READY_RETRY_ATTEMPTS {
        match docker.copy_file_from_container(container_id, path).await {
            Ok(file) => return Ok(file),
            Err(error) => {
                last_error = Some(error.to_string());
                sleep(READY_RETRY_INTERVAL).await;
            }
        }
    }

    Err(LndError::FileTimeout {
        container_id: container_id.to_string(),
        path: path.to_string(),
        attempts: READY_RETRY_ATTEMPTS,
        last_error,
    })
}

async fn init_wallet_or_read_macaroon(
    docker: &DockerClient,
    container_id: &str,
    cert_bytes: &[u8],
    socket: &str,
) -> Result<String, LndError> {
    let mut last_error = None;

    for _ in 0..READY_RETRY_ATTEMPTS {
        let init_error = match init_wallet_once(cert_bytes, socket).await {
            Ok(macaroon) if !macaroon.is_empty() => return Ok(macaroon),
            Ok(_) => Some("InitWallet returned an empty admin macaroon".to_string()),
            Err(error) => Some(error),
        };

        match docker
            .copy_file_from_container(container_id, LND_ADMIN_MACAROON_PATH)
            .await
        {
            Ok(macaroon) if !macaroon.is_empty() => return Ok(hex::encode(macaroon)),
            Ok(_) => {
                last_error = Some(format!(
                    "{LND_ADMIN_MACAROON_PATH} was empty; wallet init: {}",
                    init_error.as_deref().unwrap_or("no error")
                ));
            }
            Err(error) => {
                last_error = Some(format!(
                    "failed to read {LND_ADMIN_MACAROON_PATH}: {error}; wallet init: {}",
                    init_error.as_deref().unwrap_or("no error")
                ));
            }
        }

        sleep(READY_RETRY_INTERVAL).await;
    }

    Err(LndError::WalletInitTimeout {
        attempts: READY_RETRY_ATTEMPTS,
        last_error,
    })
}

async fn init_wallet_once(cert_bytes: &[u8], socket: &str) -> Result<String, String> {
    let channel = unauthenticated_channel(cert_bytes, socket)
        .await
        .map_err(|error| error.to_string())?;
    let mut unlocker = WalletUnlockerClient::new(channel);
    let seed = unlocker
        .gen_seed(GenSeedRequest {
            aezeed_passphrase: Vec::new(),
            seed_entropy: Vec::new(),
        })
        .await
        .map_err(|error| error.to_string())?
        .into_inner()
        .cipher_seed_mnemonic;
    let response = unlocker
        .init_wallet(InitWalletRequest {
            wallet_password: LND_WALLET_PASSWORD.to_vec(),
            cipher_seed_mnemonic: seed,
            ..Default::default()
        })
        .await
        .map_err(|error| error.to_string())?
        .into_inner();

    Ok(hex::encode(response.admin_macaroon))
}

async fn unauthenticated_channel(cert_bytes: &[u8], socket: &str) -> Result<MyChannel, LndError> {
    let uri = format!("https://{socket}")
        .parse::<Uri>()
        .map_err(|error| LndError::UnauthenticatedChannel {
            socket: socket.to_string(),
            message: error.to_string(),
        })?;

    MyChannel::new(Some(cert_bytes.to_vec()), uri)
        .await
        .map_err(|error| LndError::UnauthenticatedChannel {
            socket: socket.to_string(),
            message: error.to_string(),
        })
}

async fn wait_for_synced_get_info(
    cert_hex: &str,
    macaroon_hex: &str,
    socket: &str,
) -> Result<GetInfoResponse, LndError> {
    let mut last_error = None;

    for _ in 0..READY_RETRY_ATTEMPTS {
        match get_synced_info_once(cert_hex, macaroon_hex, socket).await {
            Ok(info) if info.synced_to_chain => return Ok(info),
            Ok(info) => {
                last_error = Some(format!(
                    "GetInfo returned synced_to_chain=false at height {}",
                    info.block_height
                ));
            }
            Err(error) => last_error = Some(error.to_string()),
        }

        sleep(READY_RETRY_INTERVAL).await;
    }

    Err(LndError::ReadyTimeout {
        attempts: READY_RETRY_ATTEMPTS,
        last_error,
    })
}

async fn get_synced_info_once(
    cert_hex: &str,
    macaroon_hex: &str,
    socket: &str,
) -> Result<GetInfoResponse, LndError> {
    let mut client = connect_authenticated(cert_hex, macaroon_hex, socket).await?;
    let info = client
        .lightning()
        .get_info(GetInfoRequest {})
        .await
        .map_err(|error| LndError::Connect {
            socket: socket.to_string(),
            message: error.to_string(),
        })?
        .into_inner();

    Ok(info)
}

async fn connect_authenticated(
    cert_hex: &str,
    macaroon_hex: &str,
    socket: &str,
) -> Result<LndClient, LndError> {
    lnd_grpc_rust::connect(
        cert_hex.to_string(),
        macaroon_hex.to_string(),
        socket.to_string(),
    )
    .await
    .map_err(|error| LndError::Connect {
        socket: socket.to_string(),
        message: error.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use crate::{
        BitcoinCore, BitcoinRpcAuth, BitcoinRpcClient, DEFAULT_LND_IMAGE,
        bitcoin::{BITCOIND_P2P_PORT, BITCOIND_RPC_PORT},
        docker::SpawnedContainer,
    };

    use super::{LND_GRPC_PORT, LND_P2P_PORT, LndConfig, lnd_args, lnd_container_spec};

    fn fake_bitcoind() -> BitcoinCore {
        BitcoinCore {
            container: SpawnedContainer {
                id: "bitcoind".to_string(),
                name: Some("bitcoind".to_string()),
                ip_address: Some("172.17.0.2".to_string()),
                host_ports: HashMap::from([(BITCOIND_RPC_PORT, 18443), (BITCOIND_P2P_PORT, 18444)]),
            },
            auth: BitcoinRpcAuth {
                user: "bitcoinrpc".to_string(),
                password: "password".to_string(),
                rpcauth: "bitcoinrpc:salt$hmac".to_string(),
            },
            rpc: BitcoinRpcClient::new("127.0.0.1", 18443, "bitcoinrpc", "password"),
            rpc_socket: "127.0.0.1:18443".to_string(),
            p2p_socket: "127.0.0.1:18444".to_string(),
        }
    }

    #[test]
    fn default_lnd_config_uses_pinned_image() {
        let config = LndConfig::new("cluster-1", "alice", 0);

        assert_eq!(config.cluster_id, "cluster-1");
        assert_eq!(config.alias, "alice");
        assert_eq!(config.node_index, 0);
        assert_eq!(config.image, DEFAULT_LND_IMAGE);
    }

    #[test]
    fn builds_lnd_args_for_bitcoind_bridge_ip() {
        let bitcoind = fake_bitcoind();
        let args = lnd_args("172.17.0.2", &bitcoind);

        assert!(args.contains(&"--bitcoin.regtest".to_string()));
        assert!(args.contains(&"--bitcoin.node=bitcoind".to_string()));
        assert!(args.contains(&"--bitcoind.rpcpolling".to_string()));
        assert!(args.contains(&"--rpclisten=0.0.0.0:10009".to_string()));
        assert!(args.contains(&"--listen=0.0.0.0:9735".to_string()));
        assert!(args.contains(&"--bitcoind.rpchost=172.17.0.2:18443".to_string()));
        assert!(args.contains(&"--bitcoind.rpcuser=bitcoinrpc".to_string()));
        assert!(args.contains(&"--bitcoind.rpcpass=password".to_string()));
        assert!(args.contains(&"--debuglevel=info".to_string()));
        assert!(args.contains(&"--noseedbackup".to_string()));
    }

    #[test]
    fn builds_lnd_container_spec() {
        let bitcoind = fake_bitcoind();
        let config = LndConfig::new("cluster-1", "alice", 0).extra_arg("--debuglevel=info");

        let spec = lnd_container_spec(&config, &bitcoind).expect("spec");

        assert_eq!(spec.name, "spawn-lnd-cluster-1-lnd-0-alice");
        assert_eq!(spec.image, DEFAULT_LND_IMAGE);
        assert!(spec.cmd.contains(&"--debuglevel=info".to_string()));
        assert!(spec.exposed_ports.contains(&LND_GRPC_PORT));
        assert!(spec.exposed_ports.contains(&LND_P2P_PORT));
    }
}
