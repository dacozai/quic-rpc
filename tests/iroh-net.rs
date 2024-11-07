#![cfg(feature = "iroh-net-transport")]

use iroh_net::{key::SecretKey, NodeAddr};
use quic_rpc::{transport, RpcClient, RpcServer};
use tokio::task::JoinHandle;

mod math;
use math::*;
mod util;

const ALPN: &[u8] = b"quic-rpc/iroh-net/test";

/// Constructs an iroh-net endpoint
///
/// ## Args
///
/// - alpn: the ALPN protocol to use
pub async fn make_endpoint(
    secret_key: SecretKey,
    alpn: &[u8],
) -> anyhow::Result<iroh_net::Endpoint> {
    iroh_net::Endpoint::builder()
        .secret_key(secret_key)
        .alpns(vec![alpn.to_vec()])
        .bind()
        .await
}

pub struct Endpoints {
    client: iroh_net::Endpoint,
    server: iroh_net::Endpoint,
    server_node_addr: NodeAddr,
}

impl Endpoints {
    pub async fn new() -> anyhow::Result<Self> {
        let server = make_endpoint(SecretKey::generate(), ALPN).await?;

        Ok(Endpoints {
            client: make_endpoint(SecretKey::generate(), ALPN).await?,
            server_node_addr: server.node_addr().await?,
            server,
        })
    }
}

fn run_server(server: iroh_net::Endpoint) -> JoinHandle<anyhow::Result<()>> {
    tokio::task::spawn(async move {
        let connection = transport::iroh_net::IrohNetListener::new(server)?;
        let server = RpcServer::new(connection);
        ComputeService::server(server).await?;
        anyhow::Ok(())
    })
}

// #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[tokio::test]
async fn iroh_net_channel_bench() -> anyhow::Result<()> {
    tracing_subscriber::fmt::try_init().ok();

    let Endpoints {
        client,
        server,
        server_node_addr,
    } = Endpoints::new().await?;
    tracing::debug!("Starting server");
    let server_handle = run_server(server);
    tracing::debug!("Starting client");

    let client = RpcClient::new(transport::iroh_net::IrohNetConnector::new(
        client,
        server_node_addr,
        ALPN.into(),
    ));
    tracing::debug!("Starting benchmark");
    bench(client, 50000).await?;
    server_handle.abort();
    Ok(())
}

#[tokio::test]
async fn iroh_net_channel_smoke() -> anyhow::Result<()> {
    tracing_subscriber::fmt::try_init().ok();
    let Endpoints {
        client,
        server,
        server_node_addr,
    } = Endpoints::new().await?;
    let server_handle = run_server(server);
    let client_connection =
        transport::iroh_net::IrohNetConnector::new(client, server_node_addr, ALPN.into());
    smoke_test(client_connection).await?;
    server_handle.abort();
    Ok(())
}

/// Test that using the client after the server goes away and comes back behaves as if the server
/// had never gone away in the first place.
///
/// This is a regression test.
#[tokio::test]
async fn server_away_and_back() -> anyhow::Result<()> {
    tracing_subscriber::fmt::try_init().ok();
    tracing::info!("Creating endpoints");

    let client = make_endpoint(SecretKey::generate(), ALPN).await?;

    let server_secret_key = SecretKey::generate();
    let server_node_id = server_secret_key.public();

    // create the RPC client
    let client_connection =
        transport::iroh_net::IrohNetConnector::<ComputeResponse, ComputeRequest>::new(
            client,
            server_node_id,
            ALPN.into(),
        );
    let client = RpcClient::<
        ComputeService,
        transport::iroh_net::IrohNetConnector<ComputeResponse, ComputeRequest>,
    >::new(client_connection);

    // send a request. No server available so it should fail
    client.rpc(Sqr(4)).await.unwrap_err();

    // create the RPC Server
    let connection = transport::iroh_net::IrohNetListener::new(
        make_endpoint(server_secret_key.clone(), ALPN).await?,
    )?;
    let server = RpcServer::new(connection);
    let server_handle = tokio::task::spawn(ComputeService::server_bounded(server, 1));

    // send the first request and wait for the response to ensure everything works as expected
    let SqrResponse(response) = client.rpc(Sqr(4)).await?;
    assert_eq!(response, 16);

    let server = server_handle.await??;
    drop(server);
    // wait for drop to free the socket
    tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;

    // make the server run again
    let connection =
        transport::iroh_net::IrohNetListener::new(make_endpoint(server_secret_key, ALPN).await?)?;
    let server = RpcServer::new(connection);
    let server_handle = tokio::task::spawn(ComputeService::server_bounded(server, 5));

    // server is running, this should work
    let SqrResponse(response) = client.rpc(Sqr(3)).await?;
    assert_eq!(response, 9);

    server_handle.abort();
    Ok(())
}
