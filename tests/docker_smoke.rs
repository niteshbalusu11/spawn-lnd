use spawn_lnd::{
    ContainerRole, ContainerSpec, DockerClient, NetworkSpec, managed_container_labels,
};
use uuid::Uuid;

#[tokio::test]
async fn docker_smoke_creates_and_cleans_labeled_container() {
    if std::env::var("RUN_DOCKER_TESTS").as_deref() != Ok("1") {
        eprintln!("skipping Docker smoke test; set RUN_DOCKER_TESTS=1 to run it");
        return;
    }

    eprintln!("docker smoke: connecting to Docker");
    let docker = DockerClient::connect().await.expect("connect to Docker");
    let cluster_id = format!("smoke-{}", Uuid::new_v4());
    let container_name = format!("spawn-lnd-{cluster_id}");
    eprintln!("docker smoke: cluster_id={cluster_id} container_name={container_name}");

    let spec = ContainerSpec::new(container_name, "hello-world:latest").labels(
        managed_container_labels(&cluster_id, ContainerRole::Bitcoind, None),
    );

    let result = async {
        eprintln!("docker smoke: creating hello-world container");
        let container = docker.create_and_start(spec).await?;
        eprintln!(
            "docker smoke: created container id={} name={:?}",
            container.id, container.name
        );
        assert!(!container.id.is_empty());
        Ok::<_, spawn_lnd::DockerError>(())
    }
    .await;

    eprintln!("docker smoke: cleaning up cluster_id={cluster_id}");
    let cleanup = docker.cleanup_cluster(&cluster_id).await;

    result.expect("create and start smoke container");
    let cleanup = cleanup.expect("cleanup smoke container");
    eprintln!(
        "docker smoke: cleanup matched={} removed={} failures={}",
        cleanup.matched,
        cleanup.removed,
        cleanup.failures.len()
    );
    assert!(
        cleanup.removed >= 1,
        "expected at least one removed container"
    );
}

#[tokio::test]
async fn docker_smoke_creates_and_cleans_labeled_network() {
    if std::env::var("RUN_DOCKER_TESTS").as_deref() != Ok("1") {
        eprintln!("skipping Docker network smoke test; set RUN_DOCKER_TESTS=1 to run it");
        return;
    }

    eprintln!("docker network smoke: connecting to Docker");
    let docker = DockerClient::connect().await.expect("connect to Docker");
    let cluster_id = format!("network-smoke-{}", Uuid::new_v4());
    eprintln!("docker network smoke: cluster_id={cluster_id}");

    let result = async {
        eprintln!("docker network smoke: creating managed network");
        let network = docker.create_network(NetworkSpec::new(&cluster_id)).await?;
        eprintln!(
            "docker network smoke: created network id={} name={} subnet={}",
            network.id, network.name, network.subnet
        );
        assert!(!network.id.is_empty());
        assert_eq!(network.name, format!("spawn-lnd-{cluster_id}"));
        assert!(network.subnet.contains('/'));
        Ok::<_, spawn_lnd::DockerError>(network)
    }
    .await;

    eprintln!("docker network smoke: cleaning up cluster_id={cluster_id}");
    let cleanup = docker.cleanup_cluster(&cluster_id).await;

    result.expect("create managed network");
    let cleanup = cleanup.expect("cleanup managed network");
    eprintln!(
        "docker network smoke: cleanup matched={} removed={} failures={}",
        cleanup.matched,
        cleanup.removed,
        cleanup.failures.len()
    );
    assert!(
        cleanup.removed >= 1,
        "expected at least one removed network"
    );
    assert!(
        docker
            .cluster_network_ids(&cluster_id)
            .await
            .expect("list cluster networks")
            .is_empty(),
        "managed network should be removed"
    );
}
