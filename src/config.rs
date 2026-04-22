use serde::{Deserialize, Serialize};
use std::{collections::HashSet, env};
use thiserror::Error;

use crate::cluster::{SpawnError, SpawnedCluster};

pub const DEFAULT_BITCOIND_IMAGE: &str = "lightninglabs/bitcoin-core:30";
pub const DEFAULT_LND_IMAGE: &str = "lightninglabs/lnd:v0.20.1-beta";
pub const DEFAULT_NODES_PER_BITCOIND: usize = 3;
pub const DEFAULT_NODE_ALIAS: &str = "node-0";
pub const DEFAULT_STARTUP_RETRY_ATTEMPTS: usize = 500;
pub const DEFAULT_STARTUP_RETRY_INTERVAL_MS: usize = 100;

pub const ENV_BITCOIND_IMAGE: &str = "SPAWN_LND_BITCOIND_IMAGE";
pub const ENV_LND_IMAGE: &str = "SPAWN_LND_LND_IMAGE";
pub const ENV_KEEP_CONTAINERS: &str = "SPAWN_LND_KEEP_CONTAINERS";
pub const ENV_NODES_PER_BITCOIND: &str = "SPAWN_LND_NODES_PER_BITCOIND";
pub const ENV_STARTUP_RETRY_ATTEMPTS: &str = "SPAWN_LND_STARTUP_RETRY_ATTEMPTS";
pub const ENV_STARTUP_RETRY_INTERVAL_MS: &str = "SPAWN_LND_STARTUP_RETRY_INTERVAL_MS";

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SpawnLndConfig {
    pub nodes: Vec<NodeConfig>,
    pub bitcoind_image: String,
    pub lnd_image: String,
    pub nodes_per_bitcoind: usize,
    pub keep_containers: bool,
    pub startup_retry: RetryPolicy,
}

impl SpawnLndConfig {
    pub fn builder() -> SpawnLndBuilder {
        SpawnLndBuilder::default()
    }

    pub async fn spawn(self) -> Result<SpawnedCluster, SpawnError> {
        SpawnedCluster::spawn(self).await
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        validate_config(self)
    }

    pub fn chain_group_count(&self) -> usize {
        if self.nodes_per_bitcoind == 0 {
            return 0;
        }

        self.nodes.len().div_ceil(self.nodes_per_bitcoind)
    }

    pub fn node_aliases(&self) -> impl Iterator<Item = &str> {
        self.nodes.iter().map(|node| node.alias.as_str())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RetryPolicy {
    pub attempts: usize,
    pub interval_ms: usize,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            attempts: DEFAULT_STARTUP_RETRY_ATTEMPTS,
            interval_ms: DEFAULT_STARTUP_RETRY_INTERVAL_MS,
        }
    }
}

impl RetryPolicy {
    pub fn new(attempts: usize, interval_ms: usize) -> Self {
        Self {
            attempts,
            interval_ms,
        }
    }

    pub fn interval(&self) -> std::time::Duration {
        std::time::Duration::from_millis(self.interval_ms as u64)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct NodeConfig {
    pub alias: String,
    pub lnd_args: Vec<String>,
}

impl NodeConfig {
    pub fn new(alias: impl Into<String>) -> Self {
        Self {
            alias: alias.into(),
            lnd_args: Vec::new(),
        }
    }

    pub fn with_lnd_arg(mut self, arg: impl Into<String>) -> Self {
        self.lnd_args.push(arg.into());
        self
    }

    pub fn with_lnd_args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.lnd_args.extend(args.into_iter().map(Into::into));
        self
    }
}

#[derive(Clone, Debug, Default)]
pub struct SpawnLnd;

impl SpawnLnd {
    pub fn builder() -> SpawnLndBuilder {
        SpawnLndConfig::builder()
    }
}

#[derive(Clone, Debug, Default)]
pub struct SpawnLndBuilder {
    nodes: Vec<NodeConfig>,
    bitcoind_image: Option<String>,
    lnd_image: Option<String>,
    nodes_per_bitcoind: Option<usize>,
    keep_containers: Option<bool>,
    startup_retry: Option<RetryPolicy>,
}

impl SpawnLndBuilder {
    pub fn node(mut self, alias: impl Into<String>) -> Self {
        self.nodes.push(NodeConfig::new(alias));
        self
    }

    pub fn node_config(mut self, node: NodeConfig) -> Self {
        self.nodes.push(node);
        self
    }

    pub fn nodes<I, S>(mut self, aliases: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.nodes.extend(aliases.into_iter().map(NodeConfig::new));
        self
    }

    pub fn bitcoind_image(mut self, image: impl Into<String>) -> Self {
        self.bitcoind_image = Some(image.into());
        self
    }

    pub fn lnd_image(mut self, image: impl Into<String>) -> Self {
        self.lnd_image = Some(image.into());
        self
    }

    pub fn nodes_per_bitcoind(mut self, count: usize) -> Self {
        self.nodes_per_bitcoind = Some(count);
        self
    }

    pub fn keep_containers(mut self, keep: bool) -> Self {
        self.keep_containers = Some(keep);
        self
    }

    pub fn startup_retry_policy(mut self, policy: RetryPolicy) -> Self {
        self.startup_retry = Some(policy);
        self
    }

    pub fn startup_retry(mut self, attempts: usize, interval_ms: usize) -> Self {
        self.startup_retry = Some(RetryPolicy::new(attempts, interval_ms));
        self
    }

    pub fn build(self) -> Result<SpawnLndConfig, ConfigError> {
        let bitcoind_image = option_or_env(
            self.bitcoind_image,
            ENV_BITCOIND_IMAGE,
            DEFAULT_BITCOIND_IMAGE,
        );
        let lnd_image = option_or_env(self.lnd_image, ENV_LND_IMAGE, DEFAULT_LND_IMAGE);
        let nodes_per_bitcoind = match self.nodes_per_bitcoind {
            Some(count) => count,
            None => env_usize(ENV_NODES_PER_BITCOIND)?.unwrap_or(DEFAULT_NODES_PER_BITCOIND),
        };
        let keep_containers = match self.keep_containers {
            Some(keep) => keep,
            None => env_bool(ENV_KEEP_CONTAINERS)?.unwrap_or(false),
        };
        let startup_retry = match self.startup_retry {
            Some(policy) => policy,
            None => RetryPolicy {
                attempts: env_usize(ENV_STARTUP_RETRY_ATTEMPTS)?
                    .unwrap_or(DEFAULT_STARTUP_RETRY_ATTEMPTS),
                interval_ms: env_usize(ENV_STARTUP_RETRY_INTERVAL_MS)?
                    .unwrap_or(DEFAULT_STARTUP_RETRY_INTERVAL_MS),
            },
        };

        let nodes = if self.nodes.is_empty() {
            vec![NodeConfig::new(DEFAULT_NODE_ALIAS)]
        } else {
            self.nodes
        };

        let config = SpawnLndConfig {
            nodes,
            bitcoind_image,
            lnd_image,
            nodes_per_bitcoind,
            keep_containers,
            startup_retry,
        };

        validate_config(&config)?;
        Ok(config)
    }

    pub async fn spawn(self) -> Result<SpawnedCluster, SpawnError> {
        self.build()?.spawn().await
    }
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum ConfigError {
    #[error("node alias cannot be empty")]
    EmptyAlias,

    #[error("node alias contains unsupported characters: {0}")]
    InvalidAlias(String),

    #[error("duplicate node alias: {0}")]
    DuplicateAlias(String),

    #[error("at least one LND node is required")]
    EmptyNodes,

    #[error("{field} Docker image cannot be empty")]
    EmptyImage { field: &'static str },

    #[error("{field} Docker image contains whitespace: {image}")]
    ImageContainsWhitespace { field: &'static str, image: String },

    #[error("{field} Docker image must include a tag or digest: {image}")]
    ImageMissingTagOrDigest { field: &'static str, image: String },

    #[error("nodes_per_bitcoind must be greater than zero")]
    InvalidNodesPerBitcoind,

    #[error("startup retry attempts must be greater than zero")]
    InvalidStartupRetryAttempts,

    #[error("startup retry interval must be greater than zero milliseconds")]
    InvalidStartupRetryInterval,

    #[error("environment variable {var} must be a positive integer, got {value}")]
    InvalidEnvUsize { var: String, value: String },

    #[error("environment variable {var} must be a boolean, got {value}")]
    InvalidEnvBool { var: String, value: String },
}

fn validate_config(config: &SpawnLndConfig) -> Result<(), ConfigError> {
    validate_image("bitcoind_image", &config.bitcoind_image)?;
    validate_image("lnd_image", &config.lnd_image)?;

    if config.nodes_per_bitcoind == 0 {
        return Err(ConfigError::InvalidNodesPerBitcoind);
    }

    if config.startup_retry.attempts == 0 {
        return Err(ConfigError::InvalidStartupRetryAttempts);
    }

    if config.startup_retry.interval_ms == 0 {
        return Err(ConfigError::InvalidStartupRetryInterval);
    }

    if config.nodes.is_empty() {
        return Err(ConfigError::EmptyNodes);
    }

    let mut aliases = HashSet::with_capacity(config.nodes.len());
    for node in &config.nodes {
        validate_alias(&node.alias)?;

        if !aliases.insert(node.alias.clone()) {
            return Err(ConfigError::DuplicateAlias(node.alias.clone()));
        }
    }

    Ok(())
}

fn validate_alias(alias: &str) -> Result<(), ConfigError> {
    if alias.is_empty() {
        return Err(ConfigError::EmptyAlias);
    }

    let is_valid = alias
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'));

    if !is_valid {
        return Err(ConfigError::InvalidAlias(alias.to_string()));
    }

    Ok(())
}

fn validate_image(field: &'static str, image: &str) -> Result<(), ConfigError> {
    if image.is_empty() {
        return Err(ConfigError::EmptyImage { field });
    }

    if image.chars().any(char::is_whitespace) {
        return Err(ConfigError::ImageContainsWhitespace {
            field,
            image: image.to_string(),
        });
    }

    if !image_has_tag_or_digest(image) {
        return Err(ConfigError::ImageMissingTagOrDigest {
            field,
            image: image.to_string(),
        });
    }

    Ok(())
}

fn image_has_tag_or_digest(image: &str) -> bool {
    if image.contains('@') {
        return true;
    }

    let last_path_component = image.rsplit('/').next().unwrap_or(image);
    last_path_component.contains(':')
}

fn option_or_env(option: Option<String>, var: &str, default: &str) -> String {
    option
        .or_else(|| env::var(var).ok())
        .unwrap_or_else(|| default.to_string())
}

fn env_usize(var: &str) -> Result<Option<usize>, ConfigError> {
    let Ok(value) = env::var(var) else {
        return Ok(None);
    };

    let parsed = value
        .parse::<usize>()
        .map_err(|_| ConfigError::InvalidEnvUsize {
            var: var.to_string(),
            value: value.clone(),
        })?;

    if parsed == 0 {
        return Err(ConfigError::InvalidEnvUsize {
            var: var.to_string(),
            value,
        });
    }

    Ok(Some(parsed))
}

fn env_bool(var: &str) -> Result<Option<bool>, ConfigError> {
    let Ok(value) = env::var(var) else {
        return Ok(None);
    };

    match value.to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(Some(true)),
        "0" | "false" | "no" | "off" => Ok(Some(false)),
        _ => Err(ConfigError::InvalidEnvBool {
            var: var.to_string(),
            value,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ConfigError, DEFAULT_BITCOIND_IMAGE, DEFAULT_LND_IMAGE, DEFAULT_NODE_ALIAS,
        DEFAULT_NODES_PER_BITCOIND, SpawnLnd, SpawnLndConfig,
    };

    #[test]
    fn builder_uses_expected_defaults() {
        let config = SpawnLnd::builder().build().expect("valid defaults");

        assert_eq!(config.bitcoind_image, DEFAULT_BITCOIND_IMAGE);
        assert_eq!(config.lnd_image, DEFAULT_LND_IMAGE);
        assert_eq!(config.nodes_per_bitcoind, DEFAULT_NODES_PER_BITCOIND);
        assert_eq!(config.keep_containers, false);
        assert_eq!(config.startup_retry, super::RetryPolicy::default());
        assert_eq!(
            config.node_aliases().collect::<Vec<_>>(),
            [DEFAULT_NODE_ALIAS]
        );
    }

    #[test]
    fn builder_accepts_custom_values() {
        let config = SpawnLndConfig::builder()
            .nodes(["alice", "bob", "carol", "dave"])
            .bitcoind_image("custom/bitcoin:30")
            .lnd_image("custom/lnd:v1")
            .nodes_per_bitcoind(3)
            .keep_containers(true)
            .startup_retry(12, 250)
            .build()
            .expect("valid config");

        assert_eq!(config.bitcoind_image, "custom/bitcoin:30");
        assert_eq!(config.lnd_image, "custom/lnd:v1");
        assert_eq!(config.nodes_per_bitcoind, 3);
        assert_eq!(config.startup_retry, super::RetryPolicy::new(12, 250));
        assert_eq!(config.chain_group_count(), 2);
        assert!(config.keep_containers);
        assert_eq!(
            config.node_aliases().collect::<Vec<_>>(),
            ["alice", "bob", "carol", "dave"]
        );
    }

    #[test]
    fn rejects_empty_alias() {
        let error = SpawnLnd::builder()
            .node("")
            .build()
            .expect_err("empty alias should fail");

        assert_eq!(error, ConfigError::EmptyAlias);
    }

    #[test]
    fn rejects_invalid_alias_characters() {
        let error = SpawnLnd::builder()
            .node("alice node")
            .build()
            .expect_err("invalid alias should fail");

        assert_eq!(error, ConfigError::InvalidAlias("alice node".to_string()));
    }

    #[test]
    fn rejects_duplicate_aliases() {
        let error = SpawnLnd::builder()
            .nodes(["alice", "bob", "alice"])
            .build()
            .expect_err("duplicate alias should fail");

        assert_eq!(error, ConfigError::DuplicateAlias("alice".to_string()));
    }

    #[test]
    fn rejects_empty_image() {
        let error = SpawnLnd::builder()
            .bitcoind_image("")
            .build()
            .expect_err("empty image should fail");

        assert_eq!(
            error,
            ConfigError::EmptyImage {
                field: "bitcoind_image"
            }
        );
    }

    #[test]
    fn rejects_untagged_image() {
        let error = SpawnLnd::builder()
            .lnd_image("lightninglabs/lnd")
            .build()
            .expect_err("untagged image should fail");

        assert_eq!(
            error,
            ConfigError::ImageMissingTagOrDigest {
                field: "lnd_image",
                image: "lightninglabs/lnd".to_string()
            }
        );
    }

    #[test]
    fn accepts_digest_pinned_image() {
        let config = SpawnLnd::builder()
            .lnd_image("lightninglabs/lnd@sha256:abc123")
            .build()
            .expect("digest-pinned image should pass");

        assert_eq!(config.lnd_image, "lightninglabs/lnd@sha256:abc123");
    }

    #[test]
    fn rejects_zero_nodes_per_bitcoind() {
        let error = SpawnLnd::builder()
            .nodes_per_bitcoind(0)
            .build()
            .expect_err("zero grouping should fail");

        assert_eq!(error, ConfigError::InvalidNodesPerBitcoind);
    }

    #[test]
    fn rejects_zero_startup_retry_attempts() {
        let error = SpawnLnd::builder()
            .startup_retry(0, 100)
            .build()
            .expect_err("zero attempts should fail");

        assert_eq!(error, ConfigError::InvalidStartupRetryAttempts);
    }

    #[test]
    fn rejects_zero_startup_retry_interval() {
        let error = SpawnLnd::builder()
            .startup_retry(1, 0)
            .build()
            .expect_err("zero interval should fail");

        assert_eq!(error, ConfigError::InvalidStartupRetryInterval);
    }

    #[test]
    fn validates_direct_config_inputs() {
        let config = SpawnLndConfig {
            nodes: Vec::new(),
            bitcoind_image: DEFAULT_BITCOIND_IMAGE.to_string(),
            lnd_image: DEFAULT_LND_IMAGE.to_string(),
            nodes_per_bitcoind: DEFAULT_NODES_PER_BITCOIND,
            keep_containers: false,
            startup_retry: super::RetryPolicy::default(),
        };

        assert_eq!(config.validate(), Err(ConfigError::EmptyNodes));
    }

    #[test]
    fn invalid_direct_config_chain_group_count_does_not_panic() {
        let config = SpawnLndConfig {
            nodes: Vec::new(),
            bitcoind_image: DEFAULT_BITCOIND_IMAGE.to_string(),
            lnd_image: DEFAULT_LND_IMAGE.to_string(),
            nodes_per_bitcoind: 0,
            keep_containers: false,
            startup_retry: super::RetryPolicy::default(),
        };

        assert_eq!(config.chain_group_count(), 0);
    }
}
