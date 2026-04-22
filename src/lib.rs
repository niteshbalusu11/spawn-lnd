#![warn(missing_docs)]

//! Docker-backed LND and Bitcoin Core regtest clusters for Rust integration
//! tests.
//!
//! The crate is library-first: integration tests should use the public API from
//! here, while binaries and examples stay thin wrappers over the library.
//!
//! # Example
//!
//! ```no_run
//! use spawn_lnd::SpawnLnd;
//!
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! let mut cluster = SpawnLnd::builder()
//!     .nodes(["alice", "bob"])
//!     .spawn()
//!     .await?;
//!
//! cluster.fund_nodes(["alice", "bob"]).await?;
//! let channel = cluster.open_channel("alice", "bob").await?;
//! let mut clients = cluster.connect_nodes().await?;
//!
//! let alice_info = clients
//!     .get_mut("alice")
//!     .expect("alice client")
//!     .lightning()
//!     .get_info(lnd_grpc_rust::lnrpc::GetInfoRequest {})
//!     .await?
//!     .into_inner();
//!
//! assert!(alice_info.synced_to_chain);
//! assert!(channel.from_channel.active);
//!
//! cluster.shutdown().await?;
//! # Ok(())
//! # }
//! ```

mod bitcoin;
mod cluster;
mod config;
mod docker;
mod lnd;

pub use bitcoin::{
    BITCOIND_P2P_PORT, BITCOIND_RPC_PORT, BitcoinCore, BitcoinCoreConfig, BitcoinCoreError,
    BitcoinRpcAuth, BitcoinRpcClient, BitcoinRpcError, BlockInfo, BlockchainInfo, CreateWallet,
    DEFAULT_BITCOIN_RPC_USER, DEFAULT_BITCOIN_WALLET_MATURITY_BLOCKS, DEFAULT_BITCOIN_WALLET_NAME,
    LoadWallet, bitcoin_core_auth_hmac, bitcoin_core_rpcauth,
};
pub use cluster::{
    ChannelReport, DEFAULT_CHANNEL_CAPACITY_SAT, DEFAULT_CHANNEL_CONFIRMATION_BLOCKS,
    DEFAULT_FUNDING_AMOUNT_BTC, DEFAULT_FUNDING_CONFIRMATION_BLOCKS, FundingReport, PeerConnection,
    SpawnError, SpawnedCluster, SpawnedNode,
};
pub use config::{
    ConfigError, DEFAULT_BITCOIND_IMAGE, DEFAULT_LND_IMAGE, DEFAULT_NODE_ALIAS,
    DEFAULT_NODES_PER_BITCOIND, DEFAULT_STARTUP_RETRY_ATTEMPTS, DEFAULT_STARTUP_RETRY_INTERVAL_MS,
    ENV_BITCOIND_IMAGE, ENV_KEEP_CONTAINERS, ENV_LND_IMAGE, ENV_NODES_PER_BITCOIND,
    ENV_STARTUP_RETRY_ATTEMPTS, ENV_STARTUP_RETRY_INTERVAL_MS, NodeConfig, RetryPolicy, SpawnLnd,
    SpawnLndBuilder, SpawnLndConfig,
};
pub use docker::{
    CleanupFailure, CleanupReport, ContainerRole, ContainerSpec, DockerClient, DockerError,
    ImageStatus, LABEL_CLUSTER, LABEL_MANAGED, LABEL_MANAGED_VALUE, LABEL_NODE, LABEL_ROLE,
    SpawnedContainer, StartupRollback, cluster_label_filters, managed_container_labels,
    managed_label_filters,
};
pub use lnd::{
    DEFAULT_GENERATE_ADDRESS, LND_ADMIN_MACAROON_PATH, LND_GRPC_PORT, LND_P2P_PORT,
    LND_TLS_CERT_PATH, LND_WALLET_PASSWORD, LndConfig, LndDaemon, LndError,
};

/// Current crate version as declared by Cargo.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg(test)]
mod tests {
    use super::VERSION;

    #[test]
    fn exposes_crate_version() {
        assert!(!VERSION.is_empty());
    }
}
