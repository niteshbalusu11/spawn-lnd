# spawn-lnd

`spawn-lnd` is a Rust library for spawning Docker-backed Bitcoin Core and LND
regtest nodes for integration tests.

The crate is library-first. It owns Docker lifecycle, daemon startup, wallet
initialization, credential extraction, and readiness checks, then returns
connection data that works with [`lnd_grpc_rust`](https://crates.io/crates/lnd_grpc_rust).

## Status

Implemented:

- Docker connection and labeled container lifecycle.
- Pull-if-missing Docker images.
- Cleanup by cluster label and rollback for partial startup failures.
- Bitcoin Core regtest spawn and JSON-RPC client.
- LND spawn, TLS cert extraction, wallet init, admin macaroon extraction.
- LND readiness that requires authenticated `GetInfo` and `synced_to_chain=true`.
- Multi-node cluster orchestration with one Bitcoin Core per three LNDs by
  default.
- Alias-keyed node metadata and `connect_nodes()` integration with
  `lnd_grpc_rust`.
- Lightning peer connection between aliases.
- Wallet funding by mining regtest coinbase blocks to an LND wallet address and
  waiting for spendable UTXOs.
- Channel opening helpers that wait for pending state, mine confirmations, and
  wait for both sides to report the channel active.

In progress:

- Minimal CLI.

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
RUN_DOCKER_TESTS=1 cargo test --test channel_smoke -- --nocapture
RUN_DOCKER_TESTS=1 cargo test --test startup_failure_smoke -- --nocapture
```

Check for leftover managed containers:

```sh
docker ps -a --filter label=spawn-lnd=true
```

Remove all managed containers:

```sh
docker rm -f $(docker ps -aq --filter label=spawn-lnd=true)
```

Keep containers for debugging failed tests:

```sh
SPAWN_LND_KEEP_CONTAINERS=1 RUN_DOCKER_TESTS=1 cargo test --test lnd_smoke -- --nocapture
```

Startup failures include a bounded Docker log tail in the typed error when a
container was created before readiness failed.

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
- `--debuglevel=info`
- `--noseedbackup`
- `--listen=0.0.0.0:9735`
- `--rpclisten=0.0.0.0:10009`
