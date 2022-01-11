use flate2::{Compress, Compression, FlushCompress, Status};
use futures_util::{Future, SinkExt, StreamExt};
use hyper::{
    server::conn::AddrStream,
    service::{make_service_fn, service_fn},
    Body, Request, Response, Server,
};
use metrics_exporter_prometheus::PrometheusHandle;
use tokio::{
    io::{AsyncRead, AsyncWrite},
    sync::{
        mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender},
        oneshot,
    },
};
use tokio_tungstenite::{
    tungstenite::{protocol::Role, Error, Message},
    WebSocketStream,
};
use tracing::{debug, error, info, trace, warn};
use twilight_gateway::shard::raw_message::Message as TwilightMessage;

use std::{convert::Infallible, net::SocketAddr, pin::Pin, sync::Arc};

use crate::{
    config::CONFIG,
    deserializer::{GatewayEvent, SequenceInfo},
    model::Identify,
    state::State,
    upgrade,
};

const HELLO: &str = r#"{"t":null,"s":null,"op":10,"d":{"heartbeat_interval":41250}}"#;
const HEARTBEAT_ACK: &str = r#"{"t":null,"s":null,"op":11,"d":null}"#;
const INVALID_SESSION: &str = r#"{"t":null,"s":null,"op":9,"d":false}"#;

const TRAILER: [u8; 4] = [0x00, 0x00, 0xff, 0xff];

fn compress_full(compressor: &mut Compress, output: &mut Vec<u8>, input: &[u8]) {
    let before_in = compressor.total_in() as usize;
    while (compressor.total_in() as usize) - before_in < input.len() {
        let offset = (compressor.total_in() as usize) - before_in;
        match compressor
            .compress_vec(&input[offset..], output, FlushCompress::None)
            .unwrap()
        {
            Status::Ok => continue,
            Status::BufError => output.reserve(4096),
            Status::StreamEnd => break,
        }
    }

    while !output.ends_with(&TRAILER) {
        output.reserve(5);
        match compressor
            .compress_vec(&[], output, FlushCompress::Sync)
            .unwrap()
        {
            Status::Ok | Status::BufError => continue,
            Status::StreamEnd => break,
        }
    }
}

async fn forward_shard(
    mut shard_id_rx: UnboundedReceiver<u64>,
    stream_writer: UnboundedSender<Message>,
    state: State,
) {
    // Wait for the client's IDENTIFY to finish and acquire the shard ID
    let shard_id = shard_id_rx.recv().await.unwrap();
    // Get a handle to the shard
    let shard_status = state.shards[shard_id as usize].clone();

    // Fake sequence number for the client. We update packets to overwrite it
    let mut seq: usize = 0;

    // Subscribe to events for this shard
    let mut event_receiver = shard_status.events.subscribe();

    debug!("[Shard {}] Starting to send events to client", shard_id);

    // Wait until we have a valid READY payload for this shard
    let ready_payload = shard_status.ready.wait_until_ready().await;

    {
        // Get a fake ready payload to send to the client
        let ready_payload = shard_status
            .guilds
            .get_ready_payload(ready_payload, &mut seq);

        if let Ok(serialized) = simd_json::to_string(&ready_payload) {
            debug!("[Shard {}] Sending newly created READY", shard_id);
            let _res = stream_writer.send(Message::Text(serialized));
        };

        // Send GUILD_CREATE/GUILD_DELETEs based on guild availability
        for payload in shard_status.guilds.get_guild_payloads(&mut seq) {
            if let Ok(serialized) = simd_json::to_string(&payload) {
                trace!(
                    "[Shard {}] Sending newly created GUILD_CREATE/GUILD_DELETE payload",
                    shard_id
                );
                let _res = stream_writer.send(Message::Text(serialized));
            };
        }
    }

    while let Ok((mut payload, sequence)) = event_receiver.recv().await {
        // Overwrite the sequence number
        if let Some(SequenceInfo(_, sequence_range)) = sequence {
            seq += 1;
            payload.replace_range(sequence_range, &seq.to_string());
        }

        let _res = stream_writer.send(Message::Text(payload));
    }
}

pub async fn handle_client<S: 'static + AsyncRead + AsyncWrite + Unpin + Send>(
    addr: SocketAddr,
    stream: S,
    state: State,
    mut use_zlib: bool,
) -> Result<(), Error> {
    // We use a oneshot channel to tell the forwarding task whether the IDENTIFY
    // contained a compression request
    let (compress_tx, compress_rx) = oneshot::channel();
    let mut compress_tx = Some(compress_tx);

    // Initialize a zlib encoder with similar settings to Discord's
    let mut compress = Compress::new(Compression::fast(), true);
    let mut compression_buffer = Vec::with_capacity(32 * 1024);

    // We need to know which shard this client is connected to in order to send messages to it
    let mut shard_status = None;

    let stream = WebSocketStream::from_raw_socket(stream, Role::Server, None).await;

    let (mut sink, mut stream) = stream.split();

    // Because we wait for IDENTIFY later, HELLO needs to be sent now
    // and optionally compressed
    if use_zlib {
        compress_full(&mut compress, &mut compression_buffer, HELLO.as_bytes());

        sink.send(Message::Binary(compression_buffer.clone()))
            .await?;
    } else {
        sink.send(Message::Text(HELLO.to_string())).await?;
    }

    // Write all messages from a queue to the sink
    let (stream_writer, mut stream_receiver) = unbounded_channel::<Message>();

    let sink_task = tokio::spawn(async move {
        if compress_rx.await.contains(&Some(true)) {
            use_zlib = true;
        }

        while let Some(msg) = stream_receiver.recv().await {
            trace!("[{}] Sending {:?}", addr, msg);

            if use_zlib {
                compression_buffer.clear();
                compress_full(&mut compress, &mut compression_buffer, &msg.into_data());

                sink.send(Message::Binary(compression_buffer.clone()))
                    .await?;
            } else {
                sink.send(msg).await?;
            }
        }

        Ok::<(), Error>(())
    });

    // Set up a task that will dump all the messages from the shard to the client
    let (shard_id_tx, shard_id_rx) = unbounded_channel();

    let shard_forward_task = tokio::spawn(forward_shard(
        shard_id_rx,
        stream_writer.clone(),
        state.clone(),
    ));

    while let Some(Ok(msg)) = stream.next().await {
        let data = msg.into_data();
        let mut payload = unsafe { String::from_utf8_unchecked(data) };

        let deserializer = match GatewayEvent::from_json(&payload) {
            Some(deserializer) => deserializer,
            None => continue,
        };

        match deserializer.op() {
            1 => {
                trace!("[{}] Sending heartbeat ACK", addr);
                let _res = stream_writer.send(Message::Text(HEARTBEAT_ACK.to_string()));
            }
            2 => {
                debug!("[{}] Client is identifying", addr);

                let identify: Identify = match simd_json::from_str(&mut payload) {
                    Ok(identify) => identify,
                    Err(e) => {
                        warn!("[{}] Invalid identify payload: {:?}", addr, e);
                        continue;
                    }
                };

                let (shard_id, shard_count) = (identify.d.shard[0], identify.d.shard[1]);

                if shard_count != state.shard_count {
                    warn!(
                        "[{}] Shard count from client identify mismatched, disconnecting",
                        addr
                    );
                    break;
                }

                if shard_id >= shard_count {
                    warn!(
                        "[{}] Shard ID from client is out of range, disconnecting",
                        addr
                    );
                    break;
                }

                if identify.d.token != CONFIG.token {
                    warn!("[{}] Token from client mismatched, disconnecting", addr);
                    break;
                }

                trace!("[{}] Shard ID is {:?}", addr, shard_id);

                // The client is connected to this shard, so prepare for sending commands to it
                shard_status = Some(state.shards[shard_id as usize].clone());

                if let Some(sender) = compress_tx.take() {
                    let _res = sender.send(identify.d.compress);
                }

                let _res = shard_id_tx.send(shard_id);
            }
            6 => {
                debug!("[{}] Client is resuming", addr);
                // TODO: Keep track of session IDs and choose one that we have active
                // This would be unnecessary if people forked their clients though
                // For now, send an invalid session so they use identify instead
                let _res = stream_writer.send(Message::text(INVALID_SESSION.to_string()));
            }
            _ => {
                if let Some(shard_status) = &shard_status {
                    trace!("[{}] Sending {:?} to Discord directly", addr, payload);
                    let _res = shard_status
                        .shard
                        .send(TwilightMessage::Text(payload))
                        .await;
                } else {
                    warn!(
                        "[{}] Client attempted to send payload before IDENTIFY",
                        addr
                    );
                }
            }
        }
    }

    debug!("[{}] Client disconnected", addr);

    sink_task.abort();
    shard_forward_task.abort();

    Ok(())
}

fn handle_metrics(
    handle: Arc<PrometheusHandle>,
) -> Pin<Box<dyn Future<Output = Result<Response<Body>, Infallible>> + Send>> {
    Box::pin(async move {
        Ok(Response::builder()
            .body(Body::from(handle.render()))
            .unwrap())
    })
}

pub async fn run(
    port: u16,
    state: State,
    metrics_handle: Arc<PrometheusHandle>,
) -> Result<(), Error> {
    let addr: SocketAddr = ([0, 0, 0, 0], port).into();

    let service = make_service_fn(move |addr: &AddrStream| {
        let state = state.clone();
        let metrics_handle = metrics_handle.clone();
        let addr = addr.remote_addr();

        trace!("[{:?}] New connection", addr);

        async move {
            Ok::<_, Infallible>(service_fn(move |incoming: Request<Body>| {
                if incoming.uri().path() == "/metrics" {
                    // Reply with metrics on /metrics
                    handle_metrics(metrics_handle.clone())
                } else {
                    // On anything else just provide the websocket server
                    Box::pin(upgrade::server(addr, incoming, state.clone()))
                }
            }))
        }
    });

    let server = Server::bind(&addr).serve(service);

    info!("Listening on {}", addr);

    if let Err(why) = server.await {
        error!("Fatal server error: {}", why);
    }

    Ok(())
}
