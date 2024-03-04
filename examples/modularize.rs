use quic_rpc::{client::RpcClient, transport::flume, RpcServer, ServiceEndpoint};
use tracing::warn;

async fn run_server(
    server_ep: impl ServiceEndpoint<app::Service>,
    handler: app::Handler,
) -> anyhow::Result<()> {
    let server = RpcServer::new(server_ep);
    loop {
        let (req, chan) = server.accept().await?;
        let handler = handler.clone();
        tokio::task::spawn(async move {
            handler.handle(req, chan).await;
        });
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let (server_conn, client_conn) = flume::connection::<app::Request, app::Response>(1);
    let server_handle = tokio::task::spawn(async move {
        let handler = app::Handler::default();
        if let Err(err) = run_server(server_conn, handler).await {
            warn!(?err, "RPC server failed");
        };
    });

    app::client_demo(client_conn).await;
    server_handle.await?;
    Ok(())
}

mod app {
    use super::{calc, clock};
    use derive_more::{From, TryInto};
    use quic_rpc::{server::RpcChannel, ServiceConnection, ServiceEndpoint};
    use serde::{Deserialize, Serialize};

    #[derive(Debug, Serialize, Deserialize, From, TryInto)]
    pub enum Request {
        Calc(calc::Request),
        Clock(clock::Request),
    }

    #[derive(Debug, Serialize, Deserialize, From, TryInto)]
    pub enum Response {
        Calc(calc::Response),
        Clock(clock::Response),
    }

    #[derive(Copy, Clone, Debug)]
    pub struct Service;
    impl quic_rpc::Service for Service {
        type Req = Request;
        type Res = Response;
    }

    #[derive(Clone, Default)]
    pub struct Handler {
        calc: calc::Handler,
        clock: clock::Handler,
    }

    impl Handler {
        pub async fn handle<E: ServiceEndpoint<Service>>(
            &self,
            req: Request,
            chan: RpcChannel<Service, E>,
        ) {
            let handler = self.clone();
            // let _res = match req {
            //     Request::Calc(req) => self.calc.handle(req, chan).await,
            //     Request::Clock(req) => self.clock.handle(req, chan).await,
            // };
        }
    }

    pub async fn client_demo<C: ServiceConnection<Service>>(conn: C) {
        // clock::client_demo(conn).await;
        // calc::client_demo(conn).await;

        // let client = quic_rpc::RpcClient::<Service, _>::new(conn);
        // let res = client.rpc(calc::AddRequest(2, 7)).await;
        // println!("rpc res: {res:?}");
    }

    impl From<calc::AddRequest> for Request {
        fn from(value: calc::AddRequest) -> Self {
            Request::Calc(calc::Request::Add(value))
        }
    }
}

mod calc {
    use derive_more::{From, TryInto};
    use quic_rpc::{message::RpcMsg, server::RpcChannel, ServiceConnection, ServiceEndpoint};
    use serde::{Deserialize, Serialize};
    use std::fmt::Debug;

    #[derive(Debug, Serialize, Deserialize)]
    pub struct AddRequest(pub i64, pub i64);

    impl RpcMsg<Service> for AddRequest {
        type Response = AddResponse;
    }

    #[derive(Debug, Serialize, Deserialize)]
    pub struct AddResponse(pub i64);

    #[derive(Debug, Serialize, Deserialize, From, TryInto)]
    pub enum Request {
        Add(AddRequest),
    }

    #[derive(Debug, Serialize, Deserialize, From, TryInto)]
    pub enum Response {
        Add(AddResponse),
    }

    #[derive(Copy, Clone, Debug)]
    pub struct Service;
    impl quic_rpc::Service for Service {
        type Req = Request;
        type Res = Response;
    }

    #[derive(Clone, Default)]
    pub struct Handler;

    impl Handler {
        pub async fn handle<E: ServiceEndpoint<Service>>(
            &self,
            req: Request,
            chan: RpcChannel<Service, E>,
        ) {
            let handler = self.clone();
            let _res = match req {
                Request::Add(req) => chan.rpc(req, handler, Self::on_add).await,
            };
        }

        pub async fn on_add(self, req: AddRequest) -> AddResponse {
            AddResponse(req.0 + req.1)
        }
    }

    pub async fn client_demo<C: ServiceConnection<Service>>(conn: C) {
        let client = quic_rpc::RpcClient::<Service, _>::new(conn);

        // a rpc call
        for i in 0..3 {
            println!("a rpc call [{i}]");
            let client = client.clone();
            tokio::task::spawn(async move {
                let res = client.rpc(AddRequest(2, i)).await;
                println!("rpc res [{i}]: {res:?}");
            });
        }
    }
}

mod clock {
    use anyhow::Result;
    use derive_more::{From, TryInto};
    use futures::{Stream, StreamExt};
    use quic_rpc::{
        message::{Msg, ServerStreaming, ServerStreamingMsg},
        server::RpcChannel,
        RpcClient, ServiceConnection, ServiceEndpoint,
    };
    use serde::{Deserialize, Serialize};
    use std::{
        fmt::Debug,
        sync::{Arc, RwLock},
        time::Duration,
    };
    use tokio::sync::Notify;

    #[derive(Debug, Serialize, Deserialize)]
    pub struct TickRequest;

    impl Msg<Service> for TickRequest {
        type Pattern = ServerStreaming;
    }

    impl ServerStreamingMsg<Service> for TickRequest {
        type Response = TickResponse;
    }

    #[derive(Debug, Serialize, Deserialize)]
    pub struct TickResponse {
        tick: usize,
    }

    #[derive(Debug, Serialize, Deserialize, From, TryInto)]
    pub enum Request {
        Tick(TickRequest),
    }

    #[derive(Debug, Serialize, Deserialize, From, TryInto)]
    pub enum Response {
        Tick(TickResponse),
    }

    #[derive(Copy, Clone, Debug)]
    pub struct Service;
    impl quic_rpc::Service for Service {
        type Req = Request;
        type Res = Response;
    }

    #[derive(Clone)]
    pub struct Handler {
        tick: Arc<RwLock<usize>>,
        ontick: Arc<Notify>,
    }

    impl Default for Handler {
        fn default() -> Self {
            Self::new(Duration::from_secs(1))
        }
    }

    impl Handler {
        pub fn new(tick_duration: Duration) -> Self {
            let h = Handler {
                tick: Default::default(),
                ontick: Default::default(),
            };
            let h2 = h.clone();
            tokio::task::spawn(async move {
                loop {
                    tokio::time::sleep(tick_duration).await;
                    *h2.tick.write().unwrap() += 1;
                    h2.ontick.notify_waiters();
                }
            });
            h
        }

        pub async fn handle<E: ServiceEndpoint<Service>>(
            &self,
            req: Request,
            chan: RpcChannel<Service, E>,
        ) {
            let handler = self.clone();
            let _res = match req {
                Request::Tick(req) => chan.server_streaming(req, handler, Self::on_tick).await,
            };
        }

        pub fn on_tick(
            self,
            req: TickRequest,
        ) -> impl Stream<Item = TickResponse> + Send + 'static {
            let (tx, rx) = flume::bounded(2);
            tokio::task::spawn(async move {
                if let Err(err) = self.on_tick0(req, tx).await {
                    tracing::warn!(?err, "on_tick RPC handler failed");
                }
            });
            rx.into_stream()
        }

        pub async fn on_tick0(
            self,
            _req: TickRequest,
            tx: flume::Sender<TickResponse>,
        ) -> Result<()> {
            loop {
                let tick = *self.tick.read().unwrap();
                tx.send_async(TickResponse { tick }).await?;
                self.ontick.notified().await;
            }
        }
    }

    pub async fn client_demo<C: ServiceConnection<Service>>(conn: C) {
        let client = RpcClient::<Service, _>::new(conn);

        // a rpc call
        for i in 0..3 {
            println!("a rpc call [{i}]");
            let client = client.clone();
            tokio::task::spawn(async move {
                if let Err(err) = run_tick(client, i).await {
                    tracing::warn!(?err, "client: run_tick failed");
                }
            });
        }
    }

    async fn run_tick<C: ServiceConnection<Service>>(
        client: RpcClient<Service, C>,
        id: usize,
    ) -> anyhow::Result<()> {
        let mut res = client.server_streaming(TickRequest).await?;
        while let Some(res) = res.next().await {
            println!("stream [{id}]: {res:?}");
        }
        println!("stream [{id}]: done");
        Ok(())
    }
}
