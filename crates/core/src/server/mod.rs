//! Handles external client connections (HTTP/WebSocket).
//!
//! This module acts as the bridge between external clients and the Freenet node's core logic.
//! It parses `ClientRequest`s, sends them to the main node event loop (`node::Node`) via an
//! internal channel, and forwards `HostResponse`s back to the clients.
//!
//! See [`../architecture.md`](../architecture.md) for its place in the overall architecture.

pub(crate) mod app_packaging;
pub(crate) mod errors;
pub(crate) mod http_gateway;
pub(crate) mod path_handlers;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, RwLock};

use freenet_stdlib::{
    client_api::{ClientError, ClientRequest, HostResponse},
    prelude::*,
};

use http_gateway::HttpGateway;
use tower_http::trace::TraceLayer;

use crate::{
    client_events::{websocket::WebSocketProxy, AuthToken, BoxedClient, ClientId, HostResult},
    config::WebsocketApiConfig,
};

use crate::server::http_gateway::AttestedContractMap;
pub use app_packaging::WebApp;

#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
pub(crate) enum ClientConnection {
    NewConnection {
        callbacks: tokio::sync::mpsc::UnboundedSender<HostCallbackResult>,
        assigned_token: Option<(AuthToken, ContractInstanceId)>,
    },
    Request {
        client_id: ClientId,
        req: Box<ClientRequest<'static>>,
        auth_token: Option<AuthToken>,
        attested_contract: Option<ContractInstanceId>,
    },
}

#[derive(Debug)]
pub(crate) enum HostCallbackResult {
    NewId {
        id: ClientId,
    },
    Result {
        id: ClientId,
        result: Result<HostResponse, ClientError>,
    },
    SubscriptionChannel {
        id: ClientId,
        key: ContractKey,
        callback: tokio::sync::mpsc::UnboundedReceiver<HostResult>,
    },
}

fn serve(socket: SocketAddr, router: axum::Router) {
    tokio::spawn(async move {
        tracing::info!("HTTP gateway listening on {}", socket);
        let listener = tokio::net::TcpListener::bind(socket).await.unwrap();
        axum::serve(listener, router).await.map_err(|e| {
            tracing::error!("Error while running HTTP gateway server: {e}");
        })
    });
}

pub mod local_node {
    use freenet_stdlib::client_api::{ClientRequest, ErrorKind};
    use std::net::{IpAddr, SocketAddr};
    use tower_http::trace::TraceLayer;

    use crate::{
        client_events::{websocket::WebSocketProxy, ClientEventsProxy, OpenRequest},
        contract::{Executor, ExecutorError},
    };

    use super::{http_gateway::HttpGateway, serve};

    pub async fn run_local_node(mut executor: Executor, socket: SocketAddr) -> anyhow::Result<()> {
        match socket.ip() {
            IpAddr::V4(ip) if !ip.is_loopback() => {
                anyhow::bail!("invalid ip: {ip}, expecting localhost")
            }
            IpAddr::V6(ip) if !ip.is_loopback() => {
                anyhow::bail!("invalid ip: {ip}, expecting localhost")
            }
            _ => {}
        }
        let (mut gw, gw_router) = HttpGateway::as_router(&socket);
        let (mut ws_proxy, ws_router) = WebSocketProxy::create_router(gw_router);

        serve(socket, ws_router.layer(TraceLayer::new_for_http()));

        // TODO: use combinator instead
        // let mut all_clients =
        //    ClientEventsCombinator::new([Box::new(ws_handle), Box::new(http_handle)]);
        enum Receiver {
            Ws,
            Gw,
        }
        let mut receiver;
        loop {
            let req = tokio::select! {
                req = ws_proxy.recv() => {
                    receiver = Receiver::Ws;
                    req?
                }
                req = gw.recv() => {
                    receiver = Receiver::Gw;
                    req?
                }
            };
            let OpenRequest {
                client_id: id,
                request,
                notification_channel,
                token,
                ..
            } = req;
            tracing::trace!(cli_id = %id, "got request -> {request}");

            let res = match *request {
                ClientRequest::ContractOp(op) => {
                    executor
                        .contract_requests(op, id, notification_channel)
                        .await
                }
                ClientRequest::DelegateOp(op) => {
                    let attested_contract = token.and_then(|token| {
                        gw.attested_contracts
                            .read()
                            .map(|guard| guard.get(&token).cloned().map(|(t, _)| t))
                            .ok()
                            .flatten()
                    });
                    executor.delegate_request(op, attested_contract.as_ref())
                }
                ClientRequest::Disconnect { cause } => {
                    if let Some(cause) = cause {
                        tracing::info!("disconnecting cause: {cause}");
                    }
                    // fixme: token must live for a bit to allow reconnections
                    if let Ok(mut guard) = gw.attested_contracts.write() {
                        if let Some(rm_token) = guard
                            .iter()
                            .find_map(|(k, (_, eid))| (eid == &id).then(|| k.clone()))
                        {
                            guard.remove(&rm_token);
                        }
                    }
                    continue;
                }
                _ => Err(ExecutorError::other(anyhow::anyhow!("not supported"))),
            };

            match res {
                Ok(res) => {
                    match receiver {
                        Receiver::Ws => ws_proxy.send(id, Ok(res)).await?,
                        Receiver::Gw => gw.send(id, Ok(res)).await?,
                    };
                }
                Err(err) if err.is_request() => {
                    let err = ErrorKind::RequestError(err.unwrap_request());
                    match receiver {
                        Receiver::Ws => {
                            ws_proxy.send(id, Err(err.into())).await?;
                        }
                        Receiver::Gw => {
                            gw.send(id, Err(err.into())).await?;
                        }
                    };
                }
                Err(err) => {
                    tracing::error!("{err}");
                    let err = Err(ErrorKind::Unhandled {
                        cause: format!("{err}").into(),
                    }
                    .into());
                    match receiver {
                        Receiver::Ws => {
                            ws_proxy.send(id, err).await?;
                        }
                        Receiver::Gw => {
                            gw.send(id, err).await?;
                        }
                    };
                }
            }
        }
    }
}

pub async fn serve_gateway(config: WebsocketApiConfig) -> [BoxedClient; 2] {
    let (gw, ws_proxy) = serve_gateway_in(config).await;
    [Box::new(gw), Box::new(ws_proxy)]
}

pub(crate) async fn serve_gateway_in(config: WebsocketApiConfig) -> (HttpGateway, WebSocketProxy) {
    let ws_socket = (config.address, config.port).into();

    // Create a shared attested_contracts map
    let attested_contracts: AttestedContractMap = Arc::new(RwLock::new(HashMap::<
        AuthToken,
        (ContractInstanceId, ClientId),
    >::new()));

    // Pass the shared map to both HttpGateway and WebSocketProxy
    let (gw, gw_router) =
        HttpGateway::as_router_with_attested_contracts(&ws_socket, attested_contracts.clone());
    let (ws_proxy, ws_router) =
        WebSocketProxy::create_router_with_attested_contracts(gw_router, attested_contracts);

    serve(ws_socket, ws_router.layer(TraceLayer::new_for_http()));
    (gw, ws_proxy)
}
