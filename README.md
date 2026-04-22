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

In progress:

- Multi-node cluster orchestration.
- Channel setup helpers.
- Minimal CLI.

## Default Images

- Bitcoin Core: `lightninglabs/bitcoin-core:30`
- LND: `lightninglabs/lnd:v0.20.1-beta`

## Example

```rust
use spawn_lnd::{BitcoinCore, BitcoinCoreConfig, DockerClient, LndConfig, LndDaemon};

#[tokio::test]
async fn spawn_one_lnd() -> Result<(), Box<dyn std::error::Error>> {
    let docker = DockerClient::connect().await?;
    let cluster_id = format!("test-{}", uuid::Uuid::new_v4());

    let result = async {
        let bitcoind = BitcoinCore::spawn(
            &docker,
            BitcoinCoreConfig::new(cluster_id.clone(), 0),
        )
        .await?;

        let lnd = LndDaemon::spawn(
            &docker,
            &bitcoind,
            LndConfig::new(cluster_id.clone(), "alice", 0),
        )
        .await?;

        let mut client = lnd.connect().await?;
        let info = client
            .lightning()
            .get_info(lnd_grpc_rust::lnrpc::GetInfoRequest {})
            .await?
            .into_inner();

        assert!(info.synced_to_chain);
        Ok::<_, Box<dyn std::error::Error>>(())
    }
    .await;

    docker.cleanup_cluster(&cluster_id).await?;
    result
}
```

## Tests

Run normal unit and gated smoke tests without Docker:

```sh
cargo test
```

Run Docker-backed smoke tests:

```sh
RUN_DOCKER_TESTS=1 cargo test --test docker_smoke -- --nocapture
RUN_DOCKER_TESTS=1 cargo test --test bitcoind_smoke -- --nocapture
RUN_DOCKER_TESTS=1 cargo test --test lnd_smoke -- --nocapture
```

Check for leftover managed containers:

```sh
docker ps -a --filter label=spawn-lnd=true
```

Remove all managed containers:

```sh
docker rm -f $(docker ps -aq --filter label=spawn-lnd=true)
```

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
