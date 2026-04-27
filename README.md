# spawn-lnd

`spawn-lnd` is a Rust library for spawning Docker-backed Bitcoin Core and LND
regtest nodes for integration tests.

The crate is library-first. It owns Docker lifecycle, daemon startup, wallet
initialization, credential extraction, and readiness checks, then returns
connection data that works with [`lnd_grpc_rust`](https://crates.io/crates/lnd_grpc_rust).

## Installation

Add the library to your crate as a dev-dependency:

```sh
cargo add spawn-lnd --dev
```

Most integration tests also need Tokio:

```sh
cargo add tokio --dev --features macros,rt-multi-thread
```

`connect_nodes()` returns raw `lnd_grpc_rust` clients, so you can call LND RPC
methods through those clients. Add `lnd_grpc_rust` as a dev-dependency only if
your test code needs to name generated protobuf types directly, such as
`lnrpc::GetInfoRequest`, `routerrpc::SendPaymentRequest`, or invoice/payment
response enums.

## Default Images

- Bitcoin Core: `lightninglabs/bitcoin-core:30`
- LND: `lightninglabs/lnd:v0.20.1-beta`

## Example

```rust
use spawn_lnd::SpawnLnd;

#[tokio::test]
async fn spawn_two_lnds() -> Result<(), Box<dyn std::error::Error>> {
    let mut cluster = SpawnLnd::builder()
        .nodes(["alice", "bob"])
        .spawn()
        .await?;

    let result = async {
        cluster.connect_peer("alice", "bob").await?;
        let funding = cluster.fund_node("alice").await?;
        assert!(funding.spendable_utxo_total_sat > 0);
        let channel = cluster.open_channel("alice", "bob").await?;
        assert!(channel.from_channel.active);

        let mut clients = cluster.connect_nodes().await?;
        let info = clients
            .get_mut("alice")
            .expect("alice")
            .lightning()
            .get_info(lnd_grpc_rust::lnrpc::GetInfoRequest {})
            .await?
            .into_inner();

        assert!(info.synced_to_chain);
        Ok::<_, Box<dyn std::error::Error>>(())
    }
    .await;

    cluster.shutdown().await?;
    result
}
```

## Tests

Docker-backed tests require a local Docker Engine that can pull:

- `lightninglabs/bitcoin-core:30`
- `lightninglabs/lnd:v0.20.1-beta`
- `hello-world:latest`

Run normal unit and gated smoke tests without Docker:

```sh
cargo test
```

Run Docker-backed smoke tests:

```sh
RUN_DOCKER_TESTS=1 cargo test --test docker_smoke -- --nocapture
RUN_DOCKER_TESTS=1 cargo test --test bitcoind_smoke -- --nocapture
RUN_DOCKER_TESTS=1 cargo test --test lnd_smoke -- --nocapture
RUN_DOCKER_TESTS=1 cargo test --test cluster_smoke -- --nocapture
RUN_DOCKER_TESTS=1 cargo test --test lifecycle_smoke -- --nocapture
RUN_DOCKER_TESTS=1 cargo test --test channel_smoke -- --nocapture
RUN_DOCKER_TESTS=1 cargo test --test e2e_smoke -- --nocapture
RUN_DOCKER_TESTS=1 cargo test --test startup_failure_smoke -- --nocapture
```

Check for leftover managed Docker resources:

```sh
docker ps -a --filter label=spawn-lnd=true
docker network ls --filter label=spawn-lnd=true
```

Remove all managed containers and networks:

```sh
docker rm -f $(docker ps -aq --filter label=spawn-lnd=true)
docker network rm $(docker network ls -q --filter label=spawn-lnd=true)
```

Keep containers for debugging failed tests:

```sh
SPAWN_LND_KEEP_CONTAINERS=1 RUN_DOCKER_TESTS=1 cargo test --test lnd_smoke -- --nocapture
```

Startup failures include a bounded Docker log tail in the typed error when a
container was created before readiness failed.

## Lifecycle Control

Clusters expose stop, start, and restart helpers for existing containers:

```rust
cluster.stop_lnd("alice").await?;
cluster.start_lnd("alice").await?;
cluster.restart_lnd("bob").await?;

cluster.stop_bitcoind(0).await?;
cluster.start_bitcoind(0).await?;
cluster.restart_bitcoind(0).await?;
```

These APIs preserve container filesystem state. They do not remove or recreate
nodes, so LND identity/wallet material and Bitcoin Core regtest data remain in
the existing containers.

## Configuration

The builder supports node aliases, image overrides, chain grouping, debug
cleanup behavior, startup retry policy, and an optional managed Docker network
subnet:

```rust
use spawn_lnd::{RetryPolicy, SpawnLnd};

let config = SpawnLnd::builder()
    .nodes(["alice", "bob"])
    .cluster_subnet("172.28.0.0/16")
    .startup_retry_policy(RetryPolicy::new(600, 100))
    .build()?;
```

By default, `spawn-lnd` chooses an explicit private subnet for each managed
cluster network, retrying alternatives if Docker reports an overlap. It then
assigns stable container IPs inside that network so Bitcoin Core and LND keep
the same bridge addresses across container restarts.

Environment overrides:

- `SPAWN_LND_BITCOIND_IMAGE`
- `SPAWN_LND_LND_IMAGE`
- `SPAWN_LND_NODES_PER_BITCOIND`
- `SPAWN_LND_KEEP_CONTAINERS`
- `SPAWN_LND_STARTUP_RETRY_ATTEMPTS`
- `SPAWN_LND_STARTUP_RETRY_INTERVAL_MS`
- `SPAWN_LND_CLUSTER_SUBNET`

## Startup Flags

Bitcoin Core uses compose-style regtest flags:

- `-regtest`
- `-printtoconsole`
- `-rpcbind=0.0.0.0`
- `-rpcallowip=0.0.0.0/0`
- `-fallbackfee=0.00001`
- `-server`
- `-txindex`
- `-blockfilterindex`
- `-coinstatsindex`
- `-rpcuser=<generated>`
- `-rpcpassword=<generated>`

LND uses:

- `--bitcoin.regtest`
- `--bitcoin.node=bitcoind`
- `--bitcoind.rpcpolling`
- `--bitcoind.rpchost=<bitcoind-bridge-ip>:18443`
- `--bitcoind.rpcuser=<generated>`
- `--bitcoind.rpcpass=<generated>`
- `--accept-keysend`
- `--allow-circular-route`
- `--debuglevel=info`
- `--noseedbackup`
- `--listen=0.0.0.0:9735`
- `--rpclisten=0.0.0.0:10009`
