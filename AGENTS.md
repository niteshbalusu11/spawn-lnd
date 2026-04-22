# AGENTS.md

## Project

`spawn-lnd` is a Rust library for integration tests that need Docker-backed
Bitcoin Core and LND regtest nodes.

## Tech Stack

- Rust 2024 edition.
- Tokio async runtime.
- Docker Engine API via `bollard`.
- Bitcoin Core JSON-RPC via `reqwest`.
- LND gRPC via public crate `lnd_grpc_rust = "2.15.0"`.
- Error types use `thiserror`.
- Docker archive extraction uses `tar`.

## Important Files

- `src/lib.rs`: public exports.
- `src/config.rs`: builder/config defaults and validation.
- `src/cluster.rs`: high-level cluster orchestration, grouping, shutdown.
- `src/docker.rs`: Docker client, labels, lifecycle, cleanup, file copy.
- `src/bitcoin.rs`: Bitcoin Core spawn, RPC auth, JSON-RPC methods.
- `src/lnd.rs`: LND spawn, wallet init, macaroon/cert extraction, readiness.
- `tests/docker_smoke.rs`: gated Docker lifecycle smoke test.
- `tests/bitcoind_smoke.rs`: gated Bitcoin Core smoke test.
- `tests/lnd_smoke.rs`: gated LND smoke test requiring `synced_to_chain=true`.
- `tests/cluster_smoke.rs`: gated two-node high-level cluster smoke test.
- `docs/todo.md`: engineering plan and remaining milestones.

## Commands

Use these before handing off changes:

```sh
cargo fmt --check
cargo check
cargo test
```

Docker-backed tests are gated:

```sh
RUN_DOCKER_TESTS=1 cargo test --test lnd_smoke -- --nocapture
RUN_DOCKER_TESTS=1 cargo test --test cluster_smoke -- --nocapture
```

Cleanup check:

```sh
docker ps -a --filter label=spawn-lnd=true
```

## Notes

- All managed Docker containers must be labeled `spawn-lnd=true`.
- Cleanup should use cluster labels and be idempotent.
- LND is not ready until authenticated `GetInfo` reports `synced_to_chain=true`.
- LND wallet init may succeed before the `WalletUnlocker` response is usable;
  fallback to `/root/.lnd/data/chain/bitcoin/regtest/admin.macaroon`.
- Do not leave Docker containers behind after tests or failed startup.
