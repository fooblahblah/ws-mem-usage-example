use axum::extract::ConnectInfo;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;
use axum_tungstenite::Error as TungsteniteError;
use axum_tungstenite::Message;
use axum_tungstenite::{WebSocket, WebSocketUpgrade};
use futures::future::try_join_all;
use futures::stream::{SplitSink, StreamExt};
use futures::SinkExt;
use log::*;
use std::error::Error;
use std::net::SocketAddr;
use std::time::Duration;
use tikv_jemallocator::Jemalloc;
use tokio::select;
use tokio::sync::mpsc::{channel, Sender};
use tokio::task::JoinHandle;
use tungstenite::protocol::frame::coding::Data;
use tungstenite::protocol::frame::coding::OpCode;
use tungstenite::protocol::frame::Frame;
use tungstenite::Result as TungsteniteResult;

#[global_allocator]
static GLOBAL: Jemalloc = Jemalloc;

type WsSink = SplitSink<WebSocket, Message>;

pub fn two_frame_fragmentation(first: &mut Frame, second: &mut Frame, first_opdata: OpCode) {
    let fh = first.header_mut();
    fh.is_final = false;
    fh.opcode = first_opdata;

    let sh = second.header_mut();
    sh.is_final = true;
    sh.opcode = OpCode::Data(Data::Continue);
}

struct FragmentedWrite {
    fragment_size: usize,
    ctl_queue: Sender<Message>,
    data_queue: Sender<Message>,
    sending_processor: JoinHandle<()>,
}

impl Drop for FragmentedWrite {
    fn drop(&mut self) {
        self.sending_processor.abort();
    }
}

impl Unpin for FragmentedWrite {}

impl FragmentedWrite {
    fn new(mut sink: WsSink, fragment_size: usize) -> Self {
        let (data_queue, mut data_queue_rx) = channel(1_000_000);
        let (ctl_queue, mut ctl_queue_rx) = channel(1_000_000);
        let sending_processor = tokio::spawn({
            async move {
                loop {
                    let res = async {
                        select! {
                            Some(msg) = data_queue_rx.recv() => {
                                if let Ok(m) = ctl_queue_rx.try_recv() {
                                    debug!("ctl try from data queue");
                                    sink.send(m).await?;
                                }

                                sink.send(msg).await?;

                                if let Ok(m) = ctl_queue_rx.try_recv() {
                                    debug!("ctl try from data queue");
                                    sink.send(m).await?;
                                }
                            },
                            Some(m) = ctl_queue_rx.recv() => {
                                debug!("ctl selected");
                                sink.send(m).await?;
                            },
                        };

                        Ok(()) as TungsteniteResult<()>
                    };

                    if let Err(e) = res.await {
                        error!("Error sending fragments: {}", e);
                    }
                }
            }
        });
        Self {
            sending_processor,
            fragment_size,
            ctl_queue,
            data_queue,
        }
    }
    async fn send(&mut self, msg: Message) -> Result<(), Box<dyn Error + '_>>
    where
        Self: Unpin,
    {
        if !(msg.is_binary() || msg.is_text()) {
            self.ctl_queue.send(msg).await?;
        } else {
            let (mut data, opdata) = match msg {
                Message::Text(d) => (d.into(), Data::Text),
                Message::Binary(d) => (d, Data::Binary),
                _ => return Ok(()),
            };

            let mut frames = vec![];

            while data.len() > 0 {
                let res: Vec<_> = data.drain(..data.len().min(self.fragment_size)).collect();
                let frame = Frame::message(res, OpCode::Data(Data::Continue), false);
                frames.push(frame);
            }

            match frames.as_mut_slice() {
                [] => {}
                [first] => {
                    let fh = first.header_mut();
                    fh.is_final = true;
                    fh.opcode = OpCode::Data(opdata);
                }
                [first, second] => {
                    two_frame_fragmentation(first, second, OpCode::Data(opdata));
                }
                [first, .., last] => {
                    two_frame_fragmentation(first, last, OpCode::Data(opdata));
                }
            };

            debug!(
                "Queued fragments: {} ({} bytes pre fragment)",
                frames.len(),
                self.fragment_size
            );
            let futs = frames
                .into_iter()
                .map(Message::Frame)
                .map(|m| self.data_queue.send(m));
            try_join_all(futs).await?;
        }
        Ok(())
    }
}

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
    let (tx_websocket, mut rx_websocket) = socket.split();
    let mut ws_write = FragmentedWrite::new(tx_websocket, 1 << 20);

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
        if let Err(e) = ws_write.send(Message::Binary(vec![1; 8 << 20])).await {
            error!("Error sending Binary payload: {}", e);
        }

        loop {
            if let Err(e) = ws_write.send(Message::Ping(Vec::new())).await {
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
