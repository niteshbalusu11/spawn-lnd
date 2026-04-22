use std::collections::HashMap;

use lnd_grpc_rust::{
    LndConnectError, LndNodeClients, LndNodeConfig,
    lnrpc::{Channel, ConnectPeerResponse},
};
use thiserror::Error;
use tokio::time::{Duration, sleep};
use uuid::Uuid;

use crate::{
    BITCOIND_P2P_PORT, BitcoinCore, BitcoinCoreConfig, BitcoinCoreError, BitcoinRpcError,
    CleanupReport, ConfigError, DEFAULT_GENERATE_ADDRESS, DockerClient, DockerError, LND_P2P_PORT,
    LndConfig, LndDaemon, LndError, NodeConfig, SpawnLndConfig, lnd::channel_point_string,
};

pub const DEFAULT_FUNDING_AMOUNT_BTC: f64 = 1.0;
pub const DEFAULT_FUNDING_CONFIRMATION_BLOCKS: u64 = 1;
pub const DEFAULT_CHANNEL_CAPACITY_SAT: i64 = 100_000;
pub const DEFAULT_CHANNEL_CONFIRMATION_BLOCKS: u64 = 6;
const SATOSHIS_PER_BTC: f64 = 100_000_000.0;
const BITCOIND_SYNC_RETRY_ATTEMPTS: usize = 500;
const BITCOIND_SYNC_RETRY_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Debug)]
pub struct SpawnedCluster {
    docker: DockerClient,
    config: SpawnLndConfig,
    cluster_id: String,
    bitcoinds: Vec<BitcoinCore>,
    nodes: HashMap<String, SpawnedNode>,
    node_order: Vec<String>,
    shutdown: bool,
}

impl SpawnedCluster {
    pub async fn spawn(config: SpawnLndConfig) -> Result<Self, SpawnError> {
        config.validate()?;
        let docker = DockerClient::connect().await?;
        Self::spawn_validated_with_docker(docker, config).await
    }

    pub async fn spawn_with_docker(
        docker: DockerClient,
        config: SpawnLndConfig,
    ) -> Result<Self, SpawnError> {
        config.validate()?;
        Self::spawn_validated_with_docker(docker, config).await
    }

    async fn spawn_validated_with_docker(
        docker: DockerClient,
        config: SpawnLndConfig,
    ) -> Result<Self, SpawnError> {
        let cluster_id = new_cluster_id();
        let cleanup_docker = docker.clone();
        let keep_containers = config.keep_containers;

        match spawn_inner(docker, config, cluster_id.clone()).await {
            Ok(cluster) => Ok(cluster),
            Err(error) => {
                if keep_containers {
                    return Err(error);
                }

                cleanup_docker
                    .cleanup_cluster(&cluster_id)
                    .await
                    .map_err(|source| SpawnError::StartupCleanup {
                        cluster_id,
                        startup_error: error.to_string(),
                        source,
                    })?;
                Err(error)
            }
        }
    }

    pub fn cluster_id(&self) -> &str {
        &self.cluster_id
    }

    pub fn config(&self) -> &SpawnLndConfig {
        &self.config
    }

    pub fn bitcoinds(&self) -> &[BitcoinCore] {
        &self.bitcoinds
    }

    pub fn node(&self, alias: &str) -> Option<&SpawnedNode> {
        self.nodes.get(alias)
    }

    pub fn nodes(&self) -> impl Iterator<Item = &SpawnedNode> {
        self.node_order
            .iter()
            .filter_map(|alias| self.nodes.get(alias))
    }

    pub fn node_aliases(&self) -> impl Iterator<Item = &str> {
        self.node_order.iter().map(String::as_str)
    }

    pub fn node_configs(&self) -> Vec<LndNodeConfig> {
        self.nodes().map(SpawnedNode::node_config).collect()
    }

    pub async fn connect_nodes(&self) -> Result<LndNodeClients, SpawnError> {
        lnd_grpc_rust::connect_nodes(self.node_configs())
            .await
            .map_err(SpawnError::ConnectNodes)
    }

    pub async fn connect_peer(
        &self,
        from_alias: &str,
        to_alias: &str,
    ) -> Result<PeerConnection, SpawnError> {
        let from = self.require_node(from_alias)?;
        let to = self.require_node(to_alias)?;
        let host = lnd_bridge_socket(to)?;
        let response = from
            .daemon
            .connect_peer(to.daemon.public_key.clone(), host.clone())
            .await
            .or_else(|error| already_connected_response(error, &to.daemon.public_key))
            .map_err(|source| SpawnError::Lnd {
                alias: from_alias.to_string(),
                source,
            })?;

        Ok(PeerConnection {
            from_alias: from_alias.to_string(),
            to_alias: to_alias.to_string(),
            public_key: to.daemon.public_key.clone(),
            socket: host,
            status: response.status,
        })
    }

    pub async fn connect_all_peers(&self) -> Result<Vec<PeerConnection>, SpawnError> {
        let mut connections = Vec::new();

        for from_alias in &self.node_order {
            for to_alias in &self.node_order {
                if from_alias == to_alias {
                    continue;
                }

                connections.push(self.connect_peer(from_alias, to_alias).await?);
            }
        }

        Ok(connections)
    }

    pub async fn fund_node(&self, alias: &str) -> Result<FundingReport, SpawnError> {
        self.fund_node_with_amount(alias, DEFAULT_FUNDING_AMOUNT_BTC)
            .await
    }

    pub async fn fund_node_with_amount(
        &self,
        alias: &str,
        amount_btc: f64,
    ) -> Result<FundingReport, SpawnError> {
        let mut reports = self.fund_nodes_with_amount([alias], amount_btc).await?;
        Ok(reports.remove(0))
    }

    pub async fn fund_nodes<I, S>(&self, aliases: I) -> Result<Vec<FundingReport>, SpawnError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        self.fund_nodes_with_amount(aliases, DEFAULT_FUNDING_AMOUNT_BTC)
            .await
    }

    pub async fn fund_nodes_with_amount<I, S>(
        &self,
        aliases: I,
        amount_btc: f64,
    ) -> Result<Vec<FundingReport>, SpawnError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let amount_sat = btc_to_sat(amount_btc)?;
        let mut recipients = Vec::new();
        let mut amounts = HashMap::new();

        for alias in aliases {
            let alias = alias.as_ref().to_string();
            let node = self.require_node(&alias)?;
            let starting_balance_sat = node
                .daemon
                .wallet_balance(1)
                .await
                .map_err(|source| SpawnError::Lnd {
                    alias: alias.clone(),
                    source,
                })?
                .confirmed_balance;
            let starting_utxos = node
                .daemon
                .list_unspent(1, i32::MAX)
                .await
                .map_err(|source| SpawnError::Lnd {
                    alias: alias.clone(),
                    source,
                })?;
            let starting_utxo_total_sat: i64 =
                starting_utxos.iter().map(|utxo| utxo.amount_sat).sum();
            let required_balance_sat = starting_balance_sat
                .checked_add(amount_sat)
                .ok_or(SpawnError::InvalidFundingAmount { amount_btc })?;
            let required_utxo_total_sat = starting_utxo_total_sat
                .checked_add(amount_sat)
                .ok_or(SpawnError::InvalidFundingAmount { amount_btc })?;
            let address = node
                .daemon
                .new_address()
                .await
                .map_err(|source| SpawnError::Lnd {
                    alias: alias.clone(),
                    source,
                })?;

            amounts.insert(address.clone(), amount_btc);
            recipients.push(FundingRecipient {
                alias,
                address,
                required_balance_sat,
                required_utxo_total_sat,
            });
        }

        if recipients.is_empty() {
            return Ok(Vec::new());
        }

        let funder = &self.bitcoinds[0];
        let txid = funder
            .wallet_rpc
            .send_many(&amounts)
            .await
            .map_err(|source| SpawnError::BitcoinRpc {
                group_index: 0,
                source,
            })?;
        let confirmation_blocks = funder
            .rpc
            .generate_to_address(
                DEFAULT_FUNDING_CONFIRMATION_BLOCKS,
                DEFAULT_GENERATE_ADDRESS,
            )
            .await
            .map_err(|source| SpawnError::BitcoinRpc {
                group_index: 0,
                source,
            })?;

        wait_bitcoind_groups_synced(&self.bitcoinds).await?;
        wait_lnd_nodes_synced(&self.nodes, &self.node_order).await?;

        let mut reports = Vec::with_capacity(recipients.len());
        for recipient in recipients {
            let node = self.require_node(&recipient.alias)?;
            let balance = node
                .daemon
                .wait_for_spendable_balance(recipient.required_balance_sat)
                .await
                .map_err(|source| SpawnError::Lnd {
                    alias: recipient.alias.clone(),
                    source,
                })?;
            let utxos = node
                .daemon
                .wait_for_spendable_utxos(recipient.required_utxo_total_sat)
                .await
                .map_err(|source| SpawnError::Lnd {
                    alias: recipient.alias.clone(),
                    source,
                })?;
            let spendable_utxo_total_sat = utxos.iter().map(|utxo| utxo.amount_sat).sum();

            reports.push(FundingReport {
                alias: recipient.alias,
                address: recipient.address,
                txid: txid.clone(),
                amount_btc,
                confirmation_blocks: confirmation_blocks.clone(),
                confirmed_balance_sat: balance.confirmed_balance,
                spendable_utxo_count: utxos.len(),
                spendable_utxo_total_sat,
            });
        }

        Ok(reports)
    }

    pub async fn open_channel(
        &self,
        from_alias: &str,
        to_alias: &str,
    ) -> Result<ChannelReport, SpawnError> {
        self.open_channel_with_amount(from_alias, to_alias, DEFAULT_CHANNEL_CAPACITY_SAT)
            .await
    }

    pub async fn open_channel_with_amount(
        &self,
        from_alias: &str,
        to_alias: &str,
        local_funding_amount_sat: i64,
    ) -> Result<ChannelReport, SpawnError> {
        let from = self.require_node(from_alias)?;
        let to = self.require_node(to_alias)?;
        let bitcoind = &self.bitcoinds[from.chain_group_index];

        self.connect_peer(from_alias, to_alias).await?;

        let channel_point = from
            .daemon
            .open_channel_sync(&to.daemon.public_key, local_funding_amount_sat, 0, true)
            .await
            .map_err(|source| SpawnError::Lnd {
                alias: from_alias.to_string(),
                source,
            })?;
        let channel_point =
            channel_point_string(&channel_point).map_err(|source| SpawnError::Lnd {
                alias: from_alias.to_string(),
                source,
            })?;

        from.daemon
            .wait_for_pending_channel(&to.daemon.public_key, &channel_point)
            .await
            .map_err(|source| SpawnError::Lnd {
                alias: from_alias.to_string(),
                source,
            })?;

        let confirmation_blocks = bitcoind
            .rpc
            .generate_to_address(
                DEFAULT_CHANNEL_CONFIRMATION_BLOCKS,
                DEFAULT_GENERATE_ADDRESS,
            )
            .await
            .map_err(|source| SpawnError::BitcoinRpc {
                group_index: from.chain_group_index,
                source,
            })?;

        wait_bitcoind_groups_synced(&self.bitcoinds).await?;
        wait_lnd_nodes_synced(&self.nodes, &self.node_order).await?;

        let from_channel = from
            .daemon
            .wait_for_active_channel(&to.daemon.public_key, &channel_point)
            .await
            .map_err(|source| SpawnError::Lnd {
                alias: from_alias.to_string(),
                source,
            })?;
        let to_channel = to
            .daemon
            .wait_for_active_channel(&from.daemon.public_key, &channel_point)
            .await
            .map_err(|source| SpawnError::Lnd {
                alias: to_alias.to_string(),
                source,
            })?;

        Ok(ChannelReport {
            from_alias: from_alias.to_string(),
            to_alias: to_alias.to_string(),
            channel_point,
            local_funding_amount_sat,
            confirmation_blocks,
            from_channel,
            to_channel,
        })
    }

    pub async fn shutdown(&mut self) -> Result<CleanupReport, SpawnError> {
        if self.shutdown || self.config.keep_containers {
            self.shutdown = true;
            return Ok(empty_cleanup_report());
        }

        let report = self.docker.cleanup_cluster(&self.cluster_id).await?;
        self.shutdown = true;
        Ok(report)
    }

    fn require_node(&self, alias: &str) -> Result<&SpawnedNode, SpawnError> {
        self.nodes
            .get(alias)
            .ok_or_else(|| SpawnError::UnknownNode {
                alias: alias.to_string(),
            })
    }
}

impl Drop for SpawnedCluster {
    fn drop(&mut self) {
        if !self.shutdown && !self.config.keep_containers {
            eprintln!(
                "spawn-lnd cluster {} dropped without shutdown(); call shutdown().await to remove managed containers",
                self.cluster_id
            );
        }
    }
}

#[derive(Clone, Debug)]
pub struct SpawnedNode {
    alias: String,
    node_index: usize,
    chain_group_index: usize,
    daemon: LndDaemon,
}

impl SpawnedNode {
    fn new(node_index: usize, chain_group_index: usize, daemon: LndDaemon) -> Self {
        Self {
            alias: daemon.alias.clone(),
            node_index,
            chain_group_index,
            daemon,
        }
    }

    pub fn alias(&self) -> &str {
        &self.alias
    }

    pub fn node_index(&self) -> usize {
        self.node_index
    }

    pub fn chain_group_index(&self) -> usize {
        self.chain_group_index
    }

    pub fn lnd(&self) -> &LndDaemon {
        &self.daemon
    }

    pub fn node_config(&self) -> LndNodeConfig {
        self.daemon.node_config()
    }

    pub fn public_key(&self) -> &str {
        &self.daemon.public_key
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PeerConnection {
    pub from_alias: String,
    pub to_alias: String,
    pub public_key: String,
    pub socket: String,
    pub status: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct FundingReport {
    pub alias: String,
    pub address: String,
    pub txid: String,
    pub amount_btc: f64,
    pub confirmation_blocks: Vec<String>,
    pub confirmed_balance_sat: i64,
    pub spendable_utxo_count: usize,
    pub spendable_utxo_total_sat: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct FundingRecipient {
    alias: String,
    address: String,
    required_balance_sat: i64,
    required_utxo_total_sat: i64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ChannelReport {
    pub from_alias: String,
    pub to_alias: String,
    pub channel_point: String,
    pub local_funding_amount_sat: i64,
    pub confirmation_blocks: Vec<String>,
    pub from_channel: Channel,
    pub to_channel: Channel,
}

#[derive(Debug, Error)]
pub enum SpawnError {
    #[error(transparent)]
    Config(#[from] ConfigError),

    #[error(transparent)]
    Docker(#[from] DockerError),

    #[error("failed to spawn Bitcoin Core chain group {group_index}")]
    BitcoinCore {
        group_index: usize,
        source: BitcoinCoreError,
    },

    #[error("failed to connect Bitcoin Core chain group {from_group} to group {to_group}")]
    BitcoinPeer {
        from_group: usize,
        to_group: usize,
        source: BitcoinRpcError,
    },

    #[error("Bitcoin Core RPC failed for chain group {group_index}")]
    BitcoinRpc {
        group_index: usize,
        source: BitcoinRpcError,
    },

    #[error(
        "Bitcoin Core chain groups did not sync to a common tip after {attempts} attempts; last tips: {last_tips:?}"
    )]
    BitcoinSyncTimeout {
        attempts: usize,
        last_tips: Vec<String>,
    },

    #[error("Bitcoin Core chain group {group_index} did not expose a bridge IP address")]
    MissingBitcoindIp { group_index: usize },

    #[error("unknown LND node alias: {alias}")]
    UnknownNode { alias: String },

    #[error("funding amount must be positive and finite, got {amount_btc} BTC")]
    InvalidFundingAmount { amount_btc: f64 },

    #[error("LND node {alias} did not expose a bridge IP address")]
    MissingLndIp { alias: String },

    #[error("failed to spawn LND node {alias}")]
    Lnd { alias: String, source: LndError },

    #[error(transparent)]
    ConnectNodes(#[from] LndConnectError),

    #[error(
        "startup failed for cluster {cluster_id}, then cleanup failed; startup error: {startup_error}"
    )]
    StartupCleanup {
        cluster_id: String,
        startup_error: String,
        source: DockerError,
    },
}

async fn spawn_inner(
    docker: DockerClient,
    config: SpawnLndConfig,
    cluster_id: String,
) -> Result<SpawnedCluster, SpawnError> {
    let bitcoinds = spawn_bitcoinds(&docker, &config, &cluster_id).await?;
    connect_bitcoind_groups(&bitcoinds).await?;
    prepare_primary_wallet(&bitcoinds).await?;
    wait_bitcoind_groups_synced(&bitcoinds).await?;
    let (nodes, node_order) = spawn_lnd_nodes(&docker, &config, &cluster_id, &bitcoinds).await?;
    wait_bitcoind_groups_synced(&bitcoinds).await?;
    wait_lnd_nodes_synced(&nodes, &node_order).await?;

    Ok(SpawnedCluster {
        docker,
        config,
        cluster_id,
        bitcoinds,
        nodes,
        node_order,
        shutdown: false,
    })
}

async fn spawn_bitcoinds(
    docker: &DockerClient,
    config: &SpawnLndConfig,
    cluster_id: &str,
) -> Result<Vec<BitcoinCore>, SpawnError> {
    let mut bitcoinds = Vec::with_capacity(config.chain_group_count());

    for group_index in 0..config.chain_group_count() {
        let bitcoind = BitcoinCore::spawn(
            docker,
            BitcoinCoreConfig::new(cluster_id, group_index).image(config.bitcoind_image.clone()),
        )
        .await
        .map_err(|source| SpawnError::BitcoinCore {
            group_index,
            source,
        })?;
        bitcoinds.push(bitcoind);
    }

    Ok(bitcoinds)
}

async fn connect_bitcoind_groups(bitcoinds: &[BitcoinCore]) -> Result<(), SpawnError> {
    for (from_group, from) in bitcoinds.iter().enumerate() {
        for (to_group, to) in bitcoinds.iter().enumerate() {
            if from_group == to_group {
                continue;
            }

            let socket = bitcoind_bridge_socket(to_group, to)?;
            from.rpc
                .add_node(&socket)
                .await
                .map_err(|source| SpawnError::BitcoinPeer {
                    from_group,
                    to_group,
                    source,
                })?;
        }
    }

    Ok(())
}

async fn prepare_primary_wallet(bitcoinds: &[BitcoinCore]) -> Result<(), SpawnError> {
    bitcoinds[0]
        .prepare_mining_wallet()
        .await
        .map_err(|source| SpawnError::BitcoinCore {
            group_index: 0,
            source,
        })?;

    Ok(())
}

async fn wait_bitcoind_groups_synced(bitcoinds: &[BitcoinCore]) -> Result<(), SpawnError> {
    if bitcoinds.len() <= 1 {
        return Ok(());
    }

    let mut last_tips = Vec::new();

    for _ in 0..BITCOIND_SYNC_RETRY_ATTEMPTS {
        let mut tips = Vec::with_capacity(bitcoinds.len());

        for (group_index, bitcoind) in bitcoinds.iter().enumerate() {
            let info = bitcoind.rpc.get_blockchain_info().await.map_err(|source| {
                SpawnError::BitcoinRpc {
                    group_index,
                    source,
                }
            })?;
            tips.push((info.blocks, info.bestblockhash));
        }

        last_tips = tips
            .iter()
            .map(|(height, hash)| format!("{height}:{hash}"))
            .collect();

        if let Some((target_height, target_hash)) = tips.iter().max_by_key(|(height, _)| *height)
            && tips
                .iter()
                .all(|(height, hash)| height == target_height && hash == target_hash)
        {
            return Ok(());
        }

        sleep(BITCOIND_SYNC_RETRY_INTERVAL).await;
    }

    Err(SpawnError::BitcoinSyncTimeout {
        attempts: BITCOIND_SYNC_RETRY_ATTEMPTS,
        last_tips,
    })
}

async fn spawn_lnd_nodes(
    docker: &DockerClient,
    config: &SpawnLndConfig,
    cluster_id: &str,
    bitcoinds: &[BitcoinCore],
) -> Result<(HashMap<String, SpawnedNode>, Vec<String>), SpawnError> {
    let mut nodes = HashMap::with_capacity(config.nodes.len());
    let mut node_order = Vec::with_capacity(config.nodes.len());

    for (node_index, node_config) in config.nodes.iter().enumerate() {
        let chain_group_index = chain_group_index(node_index, config.nodes_per_bitcoind);
        let bitcoind = &bitcoinds[chain_group_index];
        let lnd_config = lnd_config(cluster_id, node_index, node_config, &config.lnd_image);
        let daemon = LndDaemon::spawn_with_startup_cleanup(
            docker,
            bitcoind,
            lnd_config,
            !config.keep_containers,
        )
        .await
        .map_err(|source| SpawnError::Lnd {
            alias: node_config.alias.clone(),
            source,
        })?;
        wait_bitcoind_groups_synced(bitcoinds).await?;
        let node = SpawnedNode::new(node_index, chain_group_index, daemon);

        node_order.push(node.alias.clone());
        nodes.insert(node.alias.clone(), node);
    }

    Ok((nodes, node_order))
}

async fn wait_lnd_nodes_synced(
    nodes: &HashMap<String, SpawnedNode>,
    node_order: &[String],
) -> Result<(), SpawnError> {
    for alias in node_order {
        let node = &nodes[alias];
        node.daemon
            .wait_synced_to_chain()
            .await
            .map_err(|source| SpawnError::Lnd {
                alias: alias.clone(),
                source,
            })?;
    }

    Ok(())
}

fn lnd_config(
    cluster_id: &str,
    node_index: usize,
    node_config: &NodeConfig,
    image: &str,
) -> LndConfig {
    LndConfig::new(cluster_id, node_config.alias.clone(), node_index)
        .image(image)
        .extra_args(node_config.lnd_args.clone())
}

fn chain_group_index(node_index: usize, nodes_per_bitcoind: usize) -> usize {
    node_index / nodes_per_bitcoind
}

fn btc_to_sat(amount_btc: f64) -> Result<i64, SpawnError> {
    if !amount_btc.is_finite() || amount_btc <= 0.0 {
        return Err(SpawnError::InvalidFundingAmount { amount_btc });
    }

    let amount_sat = (amount_btc * SATOSHIS_PER_BTC).round();
    if amount_sat < 1.0 || amount_sat > i64::MAX as f64 {
        return Err(SpawnError::InvalidFundingAmount { amount_btc });
    }

    Ok(amount_sat as i64)
}

fn bitcoind_bridge_socket(
    group_index: usize,
    bitcoind: &BitcoinCore,
) -> Result<String, SpawnError> {
    let ip = bitcoind
        .container
        .ip_address
        .as_deref()
        .ok_or(SpawnError::MissingBitcoindIp { group_index })?;

    Ok(format!("{ip}:{BITCOIND_P2P_PORT}"))
}

fn lnd_bridge_socket(node: &SpawnedNode) -> Result<String, SpawnError> {
    let ip =
        node.daemon
            .container
            .ip_address
            .as_deref()
            .ok_or_else(|| SpawnError::MissingLndIp {
                alias: node.alias.clone(),
            })?;

    Ok(format!("{ip}:{LND_P2P_PORT}"))
}

fn already_connected_response(
    error: LndError,
    public_key: &str,
) -> Result<ConnectPeerResponse, LndError> {
    match error {
        LndError::Rpc { message, .. } if message.contains("already connected") => {
            Ok(ConnectPeerResponse {
                status: format!("already connected to {public_key}"),
            })
        }
        error => Err(error),
    }
}

fn new_cluster_id() -> String {
    format!("cluster-{}", Uuid::new_v4().simple())
}

fn empty_cleanup_report() -> CleanupReport {
    CleanupReport {
        matched: 0,
        removed: 0,
        failures: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::{already_connected_response, btc_to_sat, chain_group_index, lnd_config};
    use crate::LndError;
    use crate::NodeConfig;

    #[test]
    fn assigns_nodes_to_chain_groups() {
        let groups = (0..8)
            .map(|node_index| chain_group_index(node_index, 3))
            .collect::<Vec<_>>();

        assert_eq!(groups, [0, 0, 0, 1, 1, 1, 2, 2]);
    }

    #[test]
    fn builds_lnd_config_from_node_config() {
        let node = NodeConfig::new("alice").with_lnd_args(["--alias=Alice", "--color=#3399ff"]);
        let config = lnd_config("cluster-1", 2, &node, "custom/lnd:v1");

        assert_eq!(config.cluster_id, "cluster-1");
        assert_eq!(config.alias, "alice");
        assert_eq!(config.node_index, 2);
        assert_eq!(config.image, "custom/lnd:v1");
        assert_eq!(config.extra_args, ["--alias=Alice", "--color=#3399ff"]);
    }

    #[test]
    fn treats_already_connected_peer_as_success() {
        let response = already_connected_response(
            LndError::Rpc {
                socket: "127.0.0.1:10009".to_string(),
                method: "ConnectPeer",
                message: "already connected to peer".to_string(),
            },
            "pubkey",
        )
        .expect("already connected is success");

        assert_eq!(response.status, "already connected to pubkey");
    }

    #[test]
    fn converts_btc_amount_to_sats() {
        assert_eq!(btc_to_sat(1.0).expect("sats"), 100_000_000);
        assert_eq!(btc_to_sat(0.000_000_01).expect("sats"), 1);
        assert!(btc_to_sat(0.0).is_err());
        assert!(btc_to_sat(f64::NAN).is_err());
    }
}
