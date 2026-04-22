# spawn-lnd Engineering Plan

`spawn-lnd` is a Rust library for spawning Docker-backed Bitcoin Core and LND
regtest environments for integration tests. It should own daemon lifecycle,
test-network setup, wallet funding, and artifact discovery, then hand ready LND
connection configs to `lnd-grpc-rust`.

## Goals

- Provide a library-first API that can be used from Rust integration tests.
- Spawn Bitcoin Core and one or more LND nodes in regtest.
- Use one Bitcoin Core container per group of up to three LND nodes by default.
- Return ready-to-use `lnd_grpc_rust::LndNodeConfig` values and optional
  connected `lnd_grpc_rust::LndNodeClients`.
- Clean up Docker containers reliably, including startup rollback, explicit
  shutdown, and best-effort cleanup fallback.
- Keep configuration deterministic enough for CI but ergonomic for local tests.

## Non-Goals For The First Release

- Support Lightning implementations other than LND.
- Support non-regtest Bitcoin networks.
- Provide a long-running daemon manager CLI.
- Provide a broad production CLI. A minimal smoke-test/debug CLI is in scope.
- Implement a complete `ln-service` replacement. Only add helpers needed for
  test setup, mining, peer connection, and channel creation.

## Reference Behavior

The JavaScript reference project `ln-docker-daemons` does the following:

- Starts a Bitcoin Core regtest container with generated RPC auth.
- Waits for Bitcoin Core JSON-RPC readiness.
- Mines an initial block before starting LND.
- Starts an LND container connected to Bitcoin Core.
- Copies `/root/.lnd/tls.cert` out of the LND container.
- Initializes an LND wallet and captures the admin macaroon.
- Waits until authenticated LND gRPC calls succeed.
- Exposes helpers for block generation, Bitcoin peer connection, Lightning peer
  connection, and channel setup.

`spawn-lnd` should map this behavior to Rust types and async APIs.

## Public API Sketch

```rust
use spawn_lnd::{SpawnLnd, SpawnLndConfig};

#[tokio::test]
async fn test_with_two_nodes() -> anyhow::Result<()> {
    let cluster = SpawnLnd::builder()
        .node("alice")
        .node("bob")
        .spawn()
        .await?;

    let mut clients = cluster.connect_nodes().await?;

    let alice = clients.get_mut("alice").expect("alice exists");
    let info = alice
        .lightning()
        .get_info(lnd_grpc_rust::lnrpc::GetInfoRequest {})
        .await?;

    assert!(!info.into_inner().identity_pubkey.is_empty());

    cluster.shutdown().await?;
    Ok(())
}
```

Primary public types:

- `SpawnLnd`: builder entry point.
- `SpawnLndConfig`: resolved configuration.
- `SpawnedCluster`: owns the lifecycle for all spawned daemons.
- `SpawnedNode`: metadata and helpers for one LND node.
- `BitcoinCore`: Bitcoin Core RPC and container handle.
- `LndDaemon`: LND container handle and connection artifacts.
- `SpawnError`: typed crate error.

## Architecture

### `src/lib.rs`

- Export the public API.
- Re-export key config and handle types.
- Keep implementation modules private unless consumers need them.

### `src/config.rs`

- Define `SpawnLndConfig`, `NodeConfig`, image configuration, timeout settings,
  and chain grouping.
- Default images:
  - Bitcoin Core: `lightninglabs/bitcoin-core:30`
  - LND: `lightninglabs/lnd:v0.20.1-beta`
- Default chain grouping: one Bitcoin Core container per three LND nodes.
- Support environment overrides:
  - `SPAWN_LND_LND_IMAGE`
  - `SPAWN_LND_BITCOIND_IMAGE`
  - `SPAWN_LND_KEEP_CONTAINERS`
  - `SPAWN_LND_NODES_PER_BITCOIND`
- Validate duplicate aliases, empty aliases, invalid image names, and invalid
  grouping values.

### `src/docker.rs`

- Use `bollard` or another Docker Engine API client.
- Pull images when missing.
- Create, start, inspect, stop, and remove containers.
- Copy files from containers for TLS cert and macaroon fallback paths.
- Apply stable labels to every container:
  - `spawn-lnd=true`
  - `spawn-lnd.cluster=<cluster_id>`
  - `spawn-lnd.node=<alias>`
  - `spawn-lnd.role=<bitcoind|lnd>`
- Prefer Docker-assigned host ports and discover them from container inspect.
- Provide logs-on-error helpers for startup failures.
- Track every created container immediately so partial startup failures can
  remove already-created resources.
- Implement cleanup by cluster label as a fallback for handles that were dropped
  or failed midway through startup.

### `src/bitcoin.rs`

- Generate RPC credentials compatible with Bitcoin Core.
- Spawn Bitcoin Core with regtest arguments:
  - `-regtest`
  - `-server`
  - `-txindex`
  - `-disablewallet`
  - `-rpcbind=0.0.0.0`
  - `-rpcallowip=0.0.0.0/0`
  - `-rpcuser=<user>`
  - `-rpcpassword=<password>`
  - `-fallbackfee=0.00001`
  - `-blockfilterindex`
  - `-coinstatsindex`
- Implement a small JSON-RPC client using `reqwest`.
- Add helpers:
  - `get_blockchain_info`
  - `generate_to_address`
  - `get_block`
  - `add_node`

### `src/lnd.rs`

- Spawn LND with regtest and bitcoind backend arguments.
- Support additional caller-supplied LND args.
- Wait for `/root/.lnd/tls.cert`.
- Initialize wallet through unauthenticated WalletUnlocker gRPC.
- Capture the admin macaroon as hex.
- Wait until authenticated `GetInfo` succeeds.
- Return `lnd_grpc_rust::LndNodeConfig` for each node.

### `src/cluster.rs`

- Orchestrate Bitcoin Core and LND startup.
- Default topology: one Bitcoin Core container per group of up to three LND
  nodes.
- Connect Bitcoin Core containers to each other when more than one chain group
  exists.
- Connect Lightning peers by public key and socket.
- Provide funding helpers:
  - generate to a node wallet address
  - mine maturity blocks
  - open and confirm a channel between two aliases

### `src/error.rs`

- Use `thiserror`.
- Include enough context to debug startup failures:
  - image name
  - container id
  - node alias
  - RPC method
  - retry timeout

### `src/bin/spawn-lnd.rs`

- Provide a minimal CLI for manual smoke testing and CI diagnostics.
- Commands:
  - `spawn-lnd smoke --nodes <n>`
  - `spawn-lnd cleanup --cluster <cluster_id>`
  - `spawn-lnd cleanup --all`
- Keep the CLI thin; all real behavior should call the library.

## Milestones

### Milestone 1: Crate Shape And Configuration

- [x] Convert the crate to library-first by adding `src/lib.rs`.
- [x] Add dependencies: `tokio`, `bollard`, `futures-util`, `reqwest`,
  `serde`, `serde_json`, `thiserror`, `uuid`, `hex`, `base64`, `hmac`,
  `sha2`, `rand`, `tar`.
- [x] Add published dependency on `lnd_grpc_rust = "2.15.0"`.
- [x] Define config types and builder API.
- [x] Configure default images:
  - `lightninglabs/bitcoin-core:30`
  - `lightninglabs/lnd:v0.20.1-beta`
- [x] Validate aliases, default images, and nodes-per-bitcoind grouping.
- [x] Add unit tests for config validation.

### Milestone 2: Docker Primitives

- [x] Implement Docker connection setup.
- [x] Implement image pull-if-missing.
- [x] Implement labeled container creation.
- [x] Implement port publishing and inspect-based port discovery.
- [x] Implement stop/remove lifecycle.
- [x] Implement startup rollback for partially-created clusters.
- [x] Implement label-based cleanup for a whole cluster id.
- [x] Implement copy-file-from-container.
- [x] Add a gated Docker smoke test.

### Milestone 3: Bitcoin Core

- [x] Implement Bitcoin Core RPC credential generation.
- [x] Implement JSON-RPC client.
- [x] Spawn Bitcoin Core regtest container.
- [x] Wait for `getblockchaininfo`.
- [x] Implement `generate_to_address`.
- [x] Implement `add_node`.
- [x] Add integration test gated by `RUN_DOCKER_TESTS=1`.

### Milestone 4: Single LND Node

- [x] Spawn LND connected to Bitcoin Core.
- [x] Copy TLS cert from the container.
- [x] Add unauthenticated WalletUnlocker connection.
- [x] Generate seed and initialize wallet.
- [x] Capture admin macaroon as hex.
- [x] Wait for authenticated `GetInfo` with `synced_to_chain=true`.
- [x] Return `LndNodeConfig`.
- [x] Add integration test that connects through `lnd-grpc-rust`.

### Milestone 5: Multi-Node Cluster

- [x] Spawn N LND nodes grouped across Bitcoin Core containers at a default
  ratio of one bitcoind per three LND nodes.
- [x] Connect Bitcoin Core containers together when a cluster has multiple
  chain groups.
- [x] Allocate unique host ports and internal listener args per node.
- [x] Return alias-keyed node metadata.
- [x] Implement `connect_nodes()` using `lnd_grpc_rust::connect_nodes`.
- [x] Connect Lightning peers between aliases.
- [x] Fund node wallets and wait for mature UTXOs.
- [x] Add two-node integration test.

### Milestone 6: Channel Helpers

- [x] Implement `open_channel`.
- [x] Wait for pending channel.
- [x] Mine confirmation blocks.
- [x] Wait for active channel.
- [x] Add two-node channel integration test.

### Milestone 7: Reliability And CI

- [ ] Add startup timeouts and retry policy configuration.
- [ ] Capture useful container logs on failure.
- [ ] Make shutdown idempotent.
- [ ] Make startup failure cleanup idempotent.
- [ ] Add tests that intentionally fail during startup and assert containers are
  removed.
- [ ] Add `SPAWN_LND_KEEP_CONTAINERS=1` for debugging.
- [ ] Add CI workflow that runs unit tests and Docker-backed integration tests.
- [ ] Document Docker requirements and cleanup commands.

### Milestone 8: Minimal CLI

- [ ] Add `src/bin/spawn-lnd.rs`.
- [ ] Implement `smoke --nodes <n>` to spawn, call `GetInfo`, and shut down.
- [ ] Implement `cleanup --cluster <cluster_id>`.
- [ ] Implement `cleanup --all` for containers labeled `spawn-lnd=true`.
- [ ] Exercise the CLI smoke command in CI.

## Confirmed Design Decisions

- Use one Bitcoin Core container per three LND nodes by default.
- Use Docker-assigned host ports instead of pre-scanning free ports.
- Store cert and macaroon as hex internally to match `lnd-grpc-rust`.
- Use `bollard` instead of shelling out to the Docker CLI.
- Use the published `lnd_grpc_rust = "2.15.0"` crate.
- Use `lightninglabs/bitcoin-core:30` as the default Bitcoin Core image.
- Use `lightninglabs/lnd:v0.20.1-beta` as the default LND image.
- Include a minimal CLI for local smoke testing and cleanup.
- Make cleanup a first-class requirement: explicit shutdown, startup rollback,
  idempotent removal, label-based cleanup, and best-effort fallback.

## Open Questions

- Should `spawn-lnd` publish to crates.io as `spawn-lnd` or `spawn_lnd`?
- Should the first release expose channel helpers, or keep them behind an
  optional `channel-helpers` feature until the core lifecycle is stable?
- Should container logs stream live in debug mode, or only be captured on
  startup failure?

## Acceptance Criteria For MVP

- A user can add `spawn-lnd` as a dev-dependency.
- A Tokio integration test can spawn one LND node in regtest.
- A Tokio integration test can spawn four LND nodes and verify they are split
  across two Bitcoin Core containers.
- The test can call `GetInfo` using `lnd-grpc-rust`.
- The test can mine blocks and fund the LND wallet.
- All containers are removed after `shutdown`.
- Containers are removed when startup fails after partial creation.
- The minimal CLI can run a smoke test and clean up labeled containers.
- Failure messages identify which daemon, node alias, and startup phase failed.
