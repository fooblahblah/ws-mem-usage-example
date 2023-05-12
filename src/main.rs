use axum::extract::ConnectInfo;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;
use axum_tungstenite::Error as TungsteniteError;
use axum_tungstenite::Message;
use axum_tungstenite::{WebSocket, WebSocketUpgrade};
use futures::stream::StreamExt;
use futures::SinkExt;
use log::*;
use std::net::SocketAddr;
use std::time::Duration;
use tikv_jemallocator::Jemalloc;
use tokio::select;

#[global_allocator]
static GLOBAL: Jemalloc = Jemalloc;

#[tokio::main]
async fn main() {
    env_logger::init();

    let addr = SocketAddr::from(([0, 0, 0, 0], 8080));
    info!("listening on {addr}");

    let app = Router::new().route("/", get(ws_handler));

    axum::Server::bind(&addr)
        .serve(app.into_make_service_with_connect_info::<SocketAddr>())
        .await
        .unwrap();
}

async fn ws_handler(
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(addr, socket))
}

async fn handle_socket(addr: SocketAddr, socket: WebSocket) {
    let (mut tx_websocket, mut rx_websocket) = socket.split();

    let mut rx_handle = tokio::task::spawn(async move {
        while let Some(msg) = rx_websocket.next().await {
            let msg = match msg {
                Ok(msg) => msg,
                Err(e) => match e {
                    TungsteniteError::ConnectionClosed | TungsteniteError::AlreadyClosed => {
                        info!("Websocket connection was closed: {}", e);
                        return;
                    }
                    _ => {
                        warn!("Error reading WebSocket message: {}", e);
                        continue;
                    }
                },
            };

            trace!("Received WebSocket message: {:?}", &msg);
        }
    });

    let mut tx_handle = tokio::task::spawn(async move {
        if let Err(e) = tx_websocket.send(Message::Binary(vec![1; 8 << 20])).await {
            error!("Error sending Binary payload: {}", e);
        }

        loop {
            if let Err(e) = tx_websocket.send(Message::Ping(Vec::new())).await {
                error!("Error sending Ping: {}", e);
            }

            tokio::time::sleep(Duration::from_secs(10)).await;
        }
    });

    select! {
        _ = &mut rx_handle => {
            tx_handle.abort();
            info!("Handle socket rx connection for {addr} exited");
        },
        _ = &mut tx_handle => {
            rx_handle.abort();
            error!("Handle socket tx connection for {addr} exited");
        },
    }
}
