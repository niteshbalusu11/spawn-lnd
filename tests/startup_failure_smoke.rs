use spawn_lnd::{DockerClient, SpawnError, SpawnLnd};
use std::collections::HashSet;

#[tokio::test]
async fn startup_failure_removes_partially_created_containers()
-> Result<(), Box<dyn std::error::Error>> {
    if std::env::var("RUN_DOCKER_TESTS").as_deref() != Ok("1") {
        eprintln!("skipping startup failure smoke test; set RUN_DOCKER_TESTS=1 to run it");
        return Ok(());
    }

    eprintln!("startup failure smoke: connecting to Docker");
    let docker = DockerClient::connect().await?;
    eprintln!("startup failure smoke: snapshotting managed containers before failure");
    let before = docker
        .managed_container_ids()
        .await?
        .into_iter()
        .collect::<HashSet<_>>();
    eprintln!(
        "startup failure smoke: managed containers before={}",
        before.len()
    );

    eprintln!("startup failure smoke: spawning cluster with missing LND image");
    let result = SpawnLnd::builder()
        .node("alice")
        .lnd_image("spawn-lnd/missing-lnd-image:missing")
        .spawn()
        .await;
    eprintln!(
        "startup failure smoke: spawn result is_err={}",
        result.is_err()
    );

    assert!(result.is_err(), "invalid LND image should fail startup");
    assert!(
        matches!(result, Err(SpawnError::Lnd { alias, .. }) if alias == "alice"),
        "expected LND image failure after bitcoind startup"
    );

    let leaked = docker
        .managed_container_ids()
        .await?
        .into_iter()
        .filter(|id| !before.contains(id))
        .collect::<Vec<_>>();
    eprintln!("startup failure smoke: leaked managed containers after failed spawn={leaked:?}");
    if !leaked.is_empty() {
        eprintln!("startup failure smoke: rolling back leaked containers");
        let _ = docker.rollback_containers(leaked.clone()).await;
    }

    assert!(
        leaked.is_empty(),
        "failed cluster startup leaked managed containers: {leaked:?}"
    );

    Ok(())
}
