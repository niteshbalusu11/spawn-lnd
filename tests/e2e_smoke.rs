use std::collections::{HashMap, HashSet};

use lnd_grpc_rust::{
    LndClient, LndNodeClients,
    lnrpc::{
        ChannelGraphRequest, Invoice, Payment, PaymentFailureReason, PaymentHash,
        invoice::InvoiceState, payment::PaymentStatus,
    },
    routerrpc::SendPaymentRequest,
};
use spawn_lnd::{DEFAULT_CHANNEL_CAPACITY_SAT, SpawnLnd};
use tokio::time::{Duration, sleep};

type TestResult<T> = Result<T, Box<dyn std::error::Error>>;

#[derive(Debug)]
struct LightningPaymentReport {
    from_alias: String,
    to_alias: String,
    amount_sat: i64,
    payment_hash: String,
    payment_preimage: String,
    settled_amount_sat: i64,
}

#[tokio::test]
async fn e2e_smoke_spawns_two_bitcoinds_and_opens_channel_ring() -> TestResult<()> {
    if std::env::var("RUN_DOCKER_TESTS").as_deref() != Ok("1") {
        eprintln!("skipping e2e smoke test; set RUN_DOCKER_TESTS=1 to run it");
        return Ok(());
    }

    let aliases = ["alice", "bob", "carol", "dave"];
    let ring = [
        ("alice", "bob"),
        ("bob", "carol"),
        ("carol", "dave"),
        ("dave", "alice"),
    ];
    eprintln!("e2e smoke: spawning cluster aliases={aliases:?}");
    let mut cluster = SpawnLnd::builder().nodes(aliases).spawn().await?;
    eprintln!(
        "e2e smoke: cluster_id={} bitcoinds={} aliases={:?}",
        cluster.cluster_id(),
        cluster.bitcoinds().len(),
        cluster.node_aliases().collect::<Vec<_>>()
    );

    assert_eq!(
        cluster.bitcoinds().len(),
        2,
        "four LND nodes should be split across two bitcoinds"
    );
    assert_eq!(cluster.node_aliases().collect::<Vec<_>>(), aliases);
    assert_eq!(cluster.node("alice").expect("alice").chain_group_index(), 0);
    assert_eq!(cluster.node("bob").expect("bob").chain_group_index(), 0);
    assert_eq!(cluster.node("carol").expect("carol").chain_group_index(), 0);
    assert_eq!(cluster.node("dave").expect("dave").chain_group_index(), 1);

    let result = async {
        eprintln!("e2e smoke: batch funding aliases={aliases:?}");
        let fundings = cluster.fund_nodes(aliases).await?;
        for funding in &fundings {
            eprintln!(
                "e2e smoke: funded {} txid={} address={} spendable_utxo_total_sat={}",
                funding.alias, funding.txid, funding.address, funding.spendable_utxo_total_sat
            );
        }

        let mut channels = Vec::new();
        for (from, to) in ring {
            eprintln!(
                "e2e smoke: opening channel {from} -> {to} capacity_sat={DEFAULT_CHANNEL_CAPACITY_SAT}"
            );
            let channel = cluster.open_channel(from, to).await?;
            eprintln!(
                "e2e smoke: channel {} -> {} point={} active_from={} active_to={} blocks={:?}",
                from,
                to,
                channel.channel_point,
                channel.from_channel.active,
                channel.to_channel.active,
                channel.confirmation_blocks
            );
            channels.push(channel);
        }

        eprintln!("e2e smoke: connecting raw gRPC clients");
        let mut clients = cluster.connect_nodes().await?;
        let mut infos = Vec::new();
        for alias in aliases {
            eprintln!("e2e smoke: querying GetInfo for {alias}");
            let info = clients
                .get_mut(alias)
                .expect("client")
                .lightning()
                .get_info(lnd_grpc_rust::lnrpc::GetInfoRequest {})
                .await?
                .into_inner();
            eprintln!(
                "e2e smoke: {alias} pubkey={} synced_to_chain={} block_height={} active_channels={}",
                info.identity_pubkey,
                info.synced_to_chain,
                info.block_height,
                info.num_active_channels
            );

            infos.push((
                alias.to_string(),
                info.identity_pubkey,
                info.synced_to_chain,
                info.num_active_channels,
            ));
        }

        let public_keys: HashMap<String, String> = infos
            .iter()
            .map(|(alias, public_key, _, _)| (alias.clone(), public_key.clone()))
            .collect();
        eprintln!("e2e smoke: waiting for public graph gossip for ring edges");
        wait_for_public_graph_edges(&mut clients, &public_keys, &ring).await?;
        eprintln!("e2e smoke: public graph has all ring edges");

        let payments = vec![
            pay_invoice(&mut clients, "alice", "carol", 1_000).await?,
            pay_invoice(&mut clients, "carol", "alice", 2_000).await?,
        ];

        let mut chain_tips = Vec::new();
        for bitcoind in cluster.bitcoinds() {
            let info = bitcoind.rpc.get_blockchain_info().await?;
            chain_tips.push((info.blocks, info.bestblockhash));
        }
        eprintln!("e2e smoke: chain tips: {chain_tips:?}");

        Ok::<_, Box<dyn std::error::Error>>((fundings, channels, infos, payments, chain_tips))
    }
    .await;
    eprintln!(
        "e2e smoke: shutting down cluster_id={}",
        cluster.cluster_id()
    );
    let cleanup = cluster.shutdown().await;

    let (fundings, channels, infos, payments, chain_tips) = result?;
    let cleanup = cleanup?;
    eprintln!(
        "e2e smoke: cleanup matched={} removed={} failures={}",
        cleanup.matched,
        cleanup.removed,
        cleanup.failures.len()
    );
    assert!(
        cleanup.removed >= 6,
        "expected two bitcoinds and four LND containers to be removed"
    );

    assert_eq!(fundings.len(), aliases.len());
    let funding_txid = fundings[0].txid.clone();
    for funding in fundings {
        assert!(aliases.contains(&funding.alias.as_str()));
        assert_eq!(funding.txid, funding_txid);
        assert!(funding.spendable_utxo_total_sat >= DEFAULT_CHANNEL_CAPACITY_SAT);
    }

    assert_eq!(channels.len(), ring.len());
    for (channel, (from, to)) in channels.iter().zip(ring) {
        assert_eq!(channel.from_alias, from);
        assert_eq!(channel.to_alias, to);
        assert_eq!(
            channel.local_funding_amount_sat,
            DEFAULT_CHANNEL_CAPACITY_SAT
        );
        assert!(!channel.channel_point.is_empty());
        assert!(channel.from_channel.active);
        assert!(channel.to_channel.active);
        assert_eq!(channel.from_channel.capacity, DEFAULT_CHANNEL_CAPACITY_SAT);
        assert_eq!(channel.to_channel.capacity, DEFAULT_CHANNEL_CAPACITY_SAT);
    }

    assert_eq!(payments.len(), 2);
    assert_eq!(payments[0].from_alias, "alice");
    assert_eq!(payments[0].to_alias, "carol");
    assert_eq!(payments[0].amount_sat, 1_000);
    assert_eq!(payments[0].settled_amount_sat, 1_000);
    assert_eq!(payments[1].from_alias, "carol");
    assert_eq!(payments[1].to_alias, "alice");
    assert_eq!(payments[1].amount_sat, 2_000);
    assert_eq!(payments[1].settled_amount_sat, 2_000);
    for payment in payments {
        assert!(!payment.payment_hash.is_empty());
        assert!(!payment.payment_preimage.is_empty());
    }

    for (alias, public_key, synced_to_chain, active_channels) in infos {
        assert!(!public_key.is_empty());
        assert!(synced_to_chain, "{alias} must be synced");
        assert_eq!(
            active_channels, 2,
            "{alias} should have two active ring channels"
        );
    }

    assert_eq!(chain_tips.len(), 2);
    assert!(
        chain_tips
            .iter()
            .all(|tip| tip.0 == chain_tips[0].0 && tip.1 == chain_tips[0].1),
        "bitcoinds should end on the same tip: {chain_tips:?}"
    );
    assert!(
        chain_tips[0].0 <= 200,
        "batched funding should keep e2e block height low; tips: {chain_tips:?}"
    );

    Ok(())
}

async fn wait_for_public_graph_edges(
    clients: &mut LndNodeClients,
    public_keys: &HashMap<String, String>,
    expected_alias_edges: &[(&str, &str)],
) -> TestResult<()> {
    let mut expected_edges = HashSet::new();
    for (from_alias, to_alias) in expected_alias_edges {
        let from_key = public_keys
            .get(*from_alias)
            .ok_or_else(|| test_error(format!("missing public key for {from_alias}")))?;
        let to_key = public_keys
            .get(*to_alias)
            .ok_or_else(|| test_error(format!("missing public key for {to_alias}")))?;
        expected_edges.insert(edge_key(from_key, to_key));
    }

    let mut last_missing = Vec::new();
    for attempt in 1..=120 {
        let mut synced = true;
        last_missing.clear();

        for alias in public_keys.keys() {
            let client = clients
                .get_mut(alias)
                .ok_or_else(|| test_error(format!("missing client {alias}")))?;
            let graph = client
                .lightning()
                .describe_graph(ChannelGraphRequest {
                    include_unannounced: false,
                    include_auth_proof: false,
                })
                .await?
                .into_inner();
            let visible_edges: HashSet<_> = graph
                .edges
                .iter()
                .filter(|edge| edge.node1_policy.is_some() && edge.node2_policy.is_some())
                .map(|edge| edge_key(&edge.node1_pub, &edge.node2_pub))
                .collect();
            let missing: Vec<_> = expected_edges
                .difference(&visible_edges)
                .map(|(node_a, node_b)| format!("{node_a}<->{node_b}"))
                .collect();

            if !missing.is_empty() {
                synced = false;
                last_missing.push(format!("{alias}: {}", missing.join(", ")));
            }
        }

        if synced {
            eprintln!("e2e smoke: graph gossip synced after attempt {attempt}");
            return Ok(());
        }

        if attempt == 1 || attempt % 10 == 0 {
            eprintln!(
                "e2e smoke: waiting for graph gossip attempt={attempt} missing={}",
                last_missing.join("; ")
            );
        }
        sleep(Duration::from_millis(500)).await;
    }

    Err(test_error(format!(
        "public channel graph did not sync; missing edges: {}",
        last_missing.join("; ")
    )))
}

fn edge_key(node_a: &str, node_b: &str) -> (String, String) {
    if node_a <= node_b {
        (node_a.to_string(), node_b.to_string())
    } else {
        (node_b.to_string(), node_a.to_string())
    }
}

async fn pay_invoice(
    clients: &mut LndNodeClients,
    from_alias: &str,
    to_alias: &str,
    amount_sat: i64,
) -> TestResult<LightningPaymentReport> {
    eprintln!("e2e smoke: creating invoice {to_alias} amount_sat={amount_sat}");
    let invoice = {
        let recipient = clients
            .get_mut(to_alias)
            .ok_or_else(|| test_error(format!("missing recipient client {to_alias}")))?;

        recipient
            .lightning()
            .add_invoice(Invoice {
                memo: format!("spawn-lnd e2e {from_alias} to {to_alias}"),
                value: amount_sat,
                ..Default::default()
            })
            .await?
            .into_inner()
    };

    eprintln!(
        "e2e smoke: invoice created {to_alias} payment_hash={}",
        hex::encode(&invoice.r_hash)
    );
    assert!(
        !invoice.payment_request.is_empty(),
        "recipient should return a BOLT11 invoice"
    );

    eprintln!("e2e smoke: paying invoice {from_alias} -> {to_alias} amount_sat={amount_sat}");
    let response = {
        let payer = clients
            .get_mut(from_alias)
            .ok_or_else(|| test_error(format!("missing payer client {from_alias}")))?;
        send_payment_with_retry(payer, invoice.payment_request.clone()).await?
    };

    assert_eq!(
        response.payment_hash,
        hex::encode(&invoice.r_hash),
        "payment response hash should match the invoice hash"
    );
    assert!(
        !response.payment_preimage.is_empty(),
        "settled payment should return a preimage"
    );
    eprintln!(
        "e2e smoke: payment settled {from_alias} -> {to_alias} hash={} preimage={}",
        response.payment_hash, response.payment_preimage
    );

    let settled_invoice = {
        let recipient = clients
            .get_mut(to_alias)
            .ok_or_else(|| test_error(format!("missing recipient client {to_alias}")))?;

        recipient
            .lightning()
            .lookup_invoice(PaymentHash {
                r_hash: invoice.r_hash.clone(),
                ..Default::default()
            })
            .await?
            .into_inner()
    };

    assert_eq!(
        settled_invoice.state,
        InvoiceState::Settled as i32,
        "recipient invoice should be settled"
    );
    assert_eq!(settled_invoice.amt_paid_sat, amount_sat);
    eprintln!(
        "e2e smoke: invoice settled {to_alias} amt_paid_sat={}",
        settled_invoice.amt_paid_sat
    );

    Ok(LightningPaymentReport {
        from_alias: from_alias.to_string(),
        to_alias: to_alias.to_string(),
        amount_sat,
        payment_hash: response.payment_hash,
        payment_preimage: response.payment_preimage,
        settled_amount_sat: settled_invoice.amt_paid_sat,
    })
}

async fn send_payment_with_retry(
    payer: &mut LndClient,
    payment_request: String,
) -> TestResult<Payment> {
    let mut last_error = String::new();

    for attempt in 1..=6 {
        eprintln!("e2e smoke: SendPaymentV2 attempt={attempt}");
        let response = payer
            .router()
            .send_payment_v2(SendPaymentRequest {
                payment_request: payment_request.clone(),
                fee_limit_sat: 1_000,
                timeout_seconds: 10,
                no_inflight_updates: true,
                ..Default::default()
            })
            .await;

        match response {
            Ok(response) => {
                let mut stream = response.into_inner();
                loop {
                    match stream.message().await {
                        Ok(Some(payment)) => match payment.status {
                            status if status == PaymentStatus::Succeeded as i32 => {
                                eprintln!(
                                    "e2e smoke: SendPaymentV2 succeeded attempt={attempt} hash={}",
                                    payment.payment_hash
                                );
                                return Ok(payment);
                            }
                            status if status == PaymentStatus::Failed as i32 => {
                                last_error = format_payment_failure(&payment);
                                eprintln!(
                                    "e2e smoke: SendPaymentV2 failed attempt={attempt}: {last_error}"
                                );
                                break;
                            }
                            status => {
                                eprintln!(
                                    "e2e smoke: SendPaymentV2 update attempt={attempt} status={status}"
                                );
                            }
                        },
                        Ok(None) => {
                            last_error =
                                "payment stream ended before a terminal status".to_string();
                            eprintln!(
                                "e2e smoke: SendPaymentV2 stream ended attempt={attempt}: {last_error}"
                            );
                            break;
                        }
                        Err(err) => {
                            last_error = err.to_string();
                            eprintln!(
                                "e2e smoke: SendPaymentV2 stream error attempt={attempt}: {last_error}"
                            );
                            break;
                        }
                    }
                }
            }
            Err(err) => {
                last_error = err.to_string();
                eprintln!("e2e smoke: SendPaymentV2 RPC error attempt={attempt}: {last_error}");
            }
        }

        sleep(Duration::from_millis(500)).await;
    }

    Err(test_error(format!(
        "payment did not succeed after retries: {last_error}"
    )))
}

fn format_payment_failure(payment: &Payment) -> String {
    let reason = PaymentFailureReason::try_from(payment.failure_reason)
        .map(|reason| reason.as_str_name())
        .unwrap_or("UNKNOWN_FAILURE_REASON");

    format!("payment failed: {reason}")
}

fn test_error(message: impl Into<String>) -> Box<dyn std::error::Error> {
    Box::new(std::io::Error::other(message.into()))
}
