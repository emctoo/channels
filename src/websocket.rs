use futures::stream::SplitSink;
use futures::SinkExt;
use futures::StreamExt;
use std::fmt;
use std::fmt::{Display, Error};
use std::sync::Arc;
use tokio::select;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use warp::filters::ws::Message;

use futures::stream::SplitStream;
use redis::Client;
use serde::{Deserialize, Serialize};
use serde_tuple::{Deserialize_tuple, Serialize_tuple};
use tokio::sync::Mutex;
use tokio_stream::wrappers::UnboundedReceiverStream;
use tracing::{debug, error, info, warn};
use uuid::Uuid;
use warp::filters::ws::WebSocket;

use crate::channel::{ChannelControl, ChannelMessage};

/// reply data structures
#[derive(Clone, Debug, Serialize_tuple)]
pub struct ReplyMessage {
    pub join_reference: Option<String>, // null when it's heartbeat
    pub reference: String,
    pub topic: String, // `channel`
    pub event: String,
    pub payload: ReplyPayload,
}

#[derive(Clone, Debug, Serialize)]
pub struct ReplyPayload {
    pub status: String,
    pub response: Response,
}

// #[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
// #[serde(untagged)]
// pub enum Response {
//     Empty {},
//     Join {},
//     Heartbeat {},
//     Datetime { datetime: String, counter: u32 },
//     Message { message: String },
// }

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type")]
pub enum Response {
    #[serde(rename = "null")]
    Empty {},

    #[serde(rename = "join")]
    Join {},

    #[serde(rename = "heartbeat")]
    Heartbeat {},

    #[serde(rename = "datetime")]
    Datetime { datetime: String, counter: u32 },

    #[serde(rename = "message")]
    Message { message: String },
}

impl fmt::Display for ReplyMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Format the response based on its variant
        let response_str = match &self.payload.response {
            Response::Empty {} => "Empty".to_string(),
            Response::Join {} => "Join".to_string(),
            Response::Heartbeat {} => "Heartbeat".to_string(),
            Response::Datetime { datetime, counter } => {
                format!("<Datetime '{}' {}>", datetime, counter)
            }
            Response::Message { message } => format!("{{message: {}}}", message),
        };

        write!(
            f,
            "Message join_ref={}, ref={}, topic={}, event={}, <Payload status={}, response={}>",
            self.join_reference.clone().unwrap_or("None".to_string()),
            self.reference,
            self.topic,
            self.event,
            self.payload.status,
            response_str
        )
    }
}

// request data structures
// RequestMessage is a message from client through websocket
// it's deserialized from a JSON array
#[derive(Debug, Deserialize_tuple)]
struct RequestMessage {
    join_reference: Option<String>, // null when it's heartbeat
    reference: String,
    topic: String, // `channel`
    event: String,
    _payload: RequestPayload,
}

impl Display for RequestMessage {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> Result<(), Error> {
        write!(
            formatter,
            "<RequestMessage: join_ref={:?}, ref={}, topic={}, event={}, payload=...>",
            self.join_reference, self.reference, self.topic, self.event
        )
    }
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum RequestPayload {
    // Join { token: String },
    Leave {},
    Heartbeat {},
}

pub struct State {
    pub ctl: Mutex<ChannelControl>,
    pub redis_url: String,
    pub redis_client: redis::Client,
}

/// handle websocket connection
pub async fn on_connected(ws: WebSocket, state: Arc<State>) {
    let conn_id = Uuid::new_v4().to_string(); // 服务端生成的，内部使用
    info!("on_connected, new client: {}", conn_id);

    state
        .ctl
        .lock()
        .await
        .conn_add_publisher(conn_id.clone())
        .await;

    let (ws_tx, ws_rx) = ws.split();

    let mut ws_tx_task = tokio::spawn(websocket_tx(conn_id.clone(), state.clone(), ws_tx));
    let mut ws_rx_task = tokio::spawn(websocket_rx(ws_rx, state.clone(), conn_id.clone()));

    tokio::select! {
        _ = (&mut ws_tx_task) => ws_rx_task.abort(),
        _ = (&mut ws_rx_task) => ws_tx_task.abort(),
    }

    state
        .ctl
        .lock()
        .await
        .agent_remove(conn_id.to_string())
        .await;
    info!("client connection closed");
}

/// 把消息经由 websocket 发送到客户端: conn rx => ws tx
async fn websocket_tx(
    conn_id: String,
    state: Arc<State>,
    mut ws_tx: SplitSink<WebSocket, Message>,
) {
    debug!("launch websocket sender ...");

    // 从 ChannelControl
    let mut conn_rx = state
        .ctl
        .lock()
        .await
        .conn_subscriber(conn_id.clone())
        .await
        .unwrap();

    while let Ok(channel_message) = conn_rx.recv().await {
        if let ChannelMessage::Reply(reply_message) = channel_message {
            let text = serde_json::to_string(&reply_message).unwrap();
            let result = ws_tx.send(warp::ws::Message::text(text)).await;
            if result.is_err() {
                error!("sending failure: {}", result.err().unwrap());
                continue;
            }
        }
    }
}

/// default event handler
async fn websocket_rx(
    mut ws_rx: SplitStream<WebSocket>,
    state: Arc<State>,
    conn_id: String,
) -> redis::RedisResult<()> {
    info!("handle events ...");
    while let Some(Ok(m)) = ws_rx.next().await {
        if !m.is_text() {
            continue;
        }
        info!("input: `{}`", m.to_str().unwrap());
        let rm_result = serde_json::from_str(m.to_str().unwrap());
        if rm_result.is_err() {
            error!("error: {}", rm_result.err().unwrap());
            continue;
        }
        let rm: RequestMessage = rm_result.unwrap();

        let reference = &rm.reference;
        let join_reference = &rm.join_reference;
        let channel = &rm.topic;
        let event = &rm.event;

        if channel == "phoenix" && event == "heartbeat" {
            debug!("heartbeat message");
            reply_ok_with_empty_response(
                conn_id.clone(),
                None,
                reference,
                "phoenix",
                state.clone(),
            )
            .await;
        }

        if event == "phx_join" {
            handle_join_event(&rm, &mut ws_rx, state.clone(), &conn_id).await;
        }

        if event == "phx_leave" {
            state
                .ctl
                .lock()
                .await
                .channel_leave(channel.clone(), conn_id.to_string())
                .await
                .unwrap();
            reply_ok_with_empty_response(
                conn_id.clone(),
                join_reference.clone(),
                reference,
                channel,
                state.clone(),
            )
            .await;
        }

        // other evetns are dispatched to redis
        // dispatch_to_redis(state.redis_url.clone(), redis_topic).await?;
    }

    Ok(())
}

async fn handle_join_event(
    rm: &RequestMessage,
    _ws_rx: &mut SplitStream<WebSocket>,
    state: Arc<State>,
    conn_id: &str,
) -> JoinHandle<()> {
    let channel_name = &rm.topic; // ?

    let agent_id = format!("{}:{}", conn_id, rm.join_reference.clone().unwrap());
    info!("{} joining {} ...", agent_id, channel_name.clone(),);
    state
        .ctl
        .lock()
        .await
        .agent_add(agent_id.to_string(), None)
        .await;
    state
        .ctl
        .lock()
        .await
        .channel_join(&channel_name.clone(), agent_id.to_string())
        .await
        .unwrap();

    // task to forward from agent broadcast to conn
    let channel_forward_task = tokio::spawn(agent_to_conn(
        state.clone(),
        rm.join_reference.clone().unwrap(),
        agent_id.clone(),
        conn_id.to_string(),
    ));
    reply_ok_with_empty_response(
        conn_id.to_string().clone(),
        rm.join_reference.clone(),
        &rm.reference,
        channel_name,
        state.clone(),
    )
    .await;
    channel_forward_task
}

async fn agent_to_conn(state: Arc<State>, join_ref: String, agent_id: String, conn_id: String) {
    let mut agent_rx = state
        .ctl
        .lock()
        .await
        .agent_subscriber(agent_id.clone())
        .await
        .unwrap();
    let conn_tx = state
        .ctl
        .lock()
        .await
        .conn_publisher(conn_id.clone())
        .await
        .unwrap();
    debug!("agent {} => conn {}", agent_id.clone(), conn_id.clone());
    while let Ok(mut channel_message) = agent_rx.recv().await {
        if let ChannelMessage::Reply(ref mut reply) = channel_message {
            reply.join_reference = Some(join_ref.clone());
            let result = conn_tx.send(channel_message.clone());
            if result.is_err() {
                error!("agent {}, conn: {}, sending failure: {:?}", agent_id, conn_id, result.err().unwrap());
                break; // fails when there's no reciever, stop forwarding
            }
            debug!("F {}", channel_message);
        }
    }
}

async fn reply_ok_with_empty_response(
    conn_id: String,
    join_ref: Option<String>,
    event_ref: &str,
    channel_name: &str,
    state: Arc<State>,
) {
    let join_reply_message = ReplyMessage {
        join_reference: join_ref.clone(),
        reference: event_ref.to_string(),
        topic: channel_name.to_string(),
        event: "phx_reply".to_string(),
        payload: ReplyPayload {
            status: "ok".to_string(),
            response: Response::Empty {},
        },
    };
    let text = serde_json::to_string(&join_reply_message).unwrap();
    debug!(
        "sending empty response, channel: {}, join_ref: {:?}, ref: {}, {}",
        channel_name, join_ref, event_ref, text
    );
    state
        .ctl
        .lock()
        .await
        .conn_publish(
            conn_id.to_string(),
            ChannelMessage::Reply(join_reply_message),
        )
        .await
        .unwrap();
    debug!("sent to connection {}: {}", conn_id.clone(), text);
}

pub async fn redis_relay(
    redis_url: String,
    redis_topic: String,
    tx: mpsc::UnboundedSender<String>,
) -> redis::RedisResult<()> {
    let redis_client = Client::open(redis_url.clone())?;

    let mut redis_pubsub = redis_client.get_async_pubsub().await?;
    redis_pubsub.subscribe(redis_topic.clone()).await?;

    let mut redis_pubsub_stream = redis_pubsub.on_message();

    info!("listening to {} pubsub: `{}` ...", redis_url, redis_topic);
    loop {
        match redis_pubsub_stream.next().await {
            Some(stream_message) => {
                let payload: String = stream_message.get_payload()?;
                info!("received: {}", payload);

                if tx.send(payload).is_err() {
                    error!("receiver dropped, exiting.");
                    break;
                }
            }
            None => {
                info!("PubSub connection closed, exiting.");
                break;
            }
        }
    }

    Ok(())
}

async fn _channel_publish(
    counter: i32,
    response: Response,
    // message: String,
    state: Arc<State>,
    channel_name: &str,
    event_name: &str,
) {
    let reply_message = ReplyMessage {
        join_reference: None,
        reference: counter.to_string(),
        topic: channel_name.to_string(),
        event: event_name.to_string(),
        payload: ReplyPayload {
            status: "ok".to_string(),
            response,
        },
    };
    // unexpected error: Error("can only flatten structs and maps (got a integer)", line: 0, column: 0)
    let serialized_result = serde_json::to_string(&reply_message);
    if serialized_result.is_err() {
        error!("error: {}", serialized_result.err().unwrap());
        return;
    }
    let text = serialized_result.unwrap();
    match state
        .ctl
        .lock()
        .await
        .channel_broadcast(
            channel_name.to_string(),
            ChannelMessage::Reply(reply_message),
        )
        .await
    {
        Ok(_) => {
            debug!("published, {} > {}", event_name, text);
        }
        Err(_e) => {
            // it throws error if there's no client
            // error!(
            //     "fail to send, channel: {}, event: {}, err: {}",
            //     channel_name, event_name, e
            // );
        }
    }
}

pub async fn streaming_default_tx_handler(
    state: Arc<State>,
    mut rx: UnboundedReceiverStream<String>,
    channel_name: &str,
    event_name: &str,
) -> redis::RedisResult<()> {
    info!("launch data task ...");
    let redis_topic = format!("{}:{}", channel_name, event_name);
    let mut counter = 0;
    let redis_client = Client::open(state.redis_url.clone())?;

    let mut redis_pubsub = redis_client.get_async_pubsub().await?;
    redis_pubsub.subscribe(redis_topic.clone()).await?;
    let mut redis_pubsub_stream = redis_pubsub.on_message();
    info!("subscribe to {} {} ...", state.redis_url, redis_topic);

    loop {
        select! {
            Some(message) = rx.next() => {
                match serde_json::from_str::<Response>(&message) {
                    Err(_e) => continue,
                    Ok(response) => {
                        _channel_publish(counter, response, state.clone(), channel_name, event_name).await;
                        counter += 1;
                        debug!("publish message from memory, counter: {}", counter);
                    }
                }
            },
            optional_message = redis_pubsub_stream.next() => {
                match optional_message {
                    Some(stream_message) => {
                        let payload: String = stream_message.get_payload()?;
                        debug!("got from redis: {}", payload.clone());

                        match serde_json::from_str::<Response>(&payload) {
                            Err(e) => {
                                warn!("fail to deserialize from Redis, {}, payload: {}", e, payload);
                                continue;
                            },
                            Ok(response) => {
                                debug!("parsed from redis, response: {:?}", &response);
                                _channel_publish(counter, response, state.clone(), channel_name, event_name).await;

                                counter += 1;
                                debug!("publish message from redis, counter: {}", counter);
                            }
                        }
                    },
                    None => {
                        error!("publish message from redis, connection lost");
                        // TODO: exit and run this again?
                    }
                }
            }
        }
    }
}

// 每秒发送一个时间戳
pub async fn system_default_tx_handler(state: Arc<State>, channel_name: &str) {
    tokio::time::sleep(tokio::time::Duration::from_secs(10)).await;

    info!("launch system/datetime task...");
    let mut counter = 0;
    let event = "datetime";
    loop {
        let now = chrono::Local::now();
        let message = ReplyMessage {
            join_reference: None,
            reference: counter.to_string(),
            topic: channel_name.to_string(),
            event: event.to_string(),
            payload: ReplyPayload {
                status: "ok".to_string(),
                response: Response::Datetime {
                    datetime: now.to_rfc3339_opts(chrono::SecondsFormat::Millis, false),
                    counter,
                },
            },
        };
        // let text = serde_json::to_string(&message).unwrap();
        match state
            .ctl
            .lock()
            .await
            .channel_broadcast(channel_name.to_string(), ChannelMessage::Reply(message))
            .await
        {
            Ok(0) => {} // no client
            Ok(_) => {} // debug!("datetime > {}", text),
            Err(_e) => {
                // FIXME: when thers's no client, it's an error
                // error!("`{}` `{}`, {}, {}", channel_name, event, e, text)
            }
        }

        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
        counter += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::{SinkExt, StreamExt};
    use std::sync::Arc;
    use tokio::sync::Mutex;
    use tokio_tungstenite::connect_async;
    use tokio_tungstenite::tungstenite::Message;
    use warp::Filter;

    async fn setup_test_server() -> (String, Arc<State>) {
        let redis_url = "redis://localhost:6379".to_string();
        let redis_client = redis::Client::open(redis_url.clone()).unwrap();
        let state = Arc::new(State {
            ctl: Mutex::new(ChannelControl::new()),
            redis_url,
            redis_client,
        });

        // Setup channels
        state
            .ctl
            .lock()
            .await
            .channel_add("phoenix".into(), None)
            .await;
        state
            .ctl
            .lock()
            .await
            .channel_add("system".into(), None)
            .await;
        state
            .ctl
            .lock()
            .await
            .channel_add("streaming".into(), None)
            .await;

        // Spawn system task
        tokio::spawn(system_default_tx_handler(state.clone(), "system"));

        let websocket_shared_state = state.clone();
        let websocket_shared_state = warp::any().map(move || websocket_shared_state.clone());
        let ws = warp::path("websocket")
            .and(warp::ws())
            .and(websocket_shared_state)
            .map(|ws: warp::ws::Ws, state| {
                ws.on_upgrade(move |socket| on_connected(socket, state))
            });

        let (addr, server) = warp::serve(ws).bind_ephemeral(([127, 0, 0, 1], 0));
        let addr = format!("ws://127.0.0.1:{}/websocket", addr.port());
        tokio::spawn(server);

        (addr, state)
    }

    async fn connect_client(
        addr: &str,
    ) -> (
        futures::stream::SplitSink<
            tokio_tungstenite::WebSocketStream<
                tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
            >,
            tokio_tungstenite::tungstenite::Message,
        >,
        futures::stream::SplitStream<
            tokio_tungstenite::WebSocketStream<
                tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
            >,
        >,
    ) {
        let (ws_stream, _) = connect_async(addr).await.expect("Failed to connect");
        ws_stream.split()
    }

    #[tokio::test]
    async fn test_websocket_connection() {
        let (addr, _) = setup_test_server().await;
        let (mut tx, mut rx) = connect_client(&addr).await;

        // Test initial connection with heartbeat
        let heartbeat = r#"[null,"1","phoenix","heartbeat",{}]"#;
        tx.send(Message::text(heartbeat)).await.unwrap();

        if let Some(Ok(msg)) = rx.next().await {
            let response: serde_json::Value = serde_json::from_str(&msg.to_string()).unwrap();
            assert_eq!(response[2], "phoenix");
            assert_eq!(response[4]["status"], "ok");
        }
    }

    // FIXME: not cleaned up
    //
    // #[tokio::test]
    // async fn test_channel_join_leave_flow() {
    //     let (addr, state) = setup_test_server().await;
    //     let (mut tx, mut rx) = connect_client(&addr).await;
    //
    //     // Join system channel
    //     let join_msg = r#"["1","ref1","system","phx_join",{"token":"test"}]"#;
    //     tx.send(Message::text(join_msg)).await.unwrap();
    //
    //     // Verify join response
    //     if let Some(Ok(msg)) = rx.next().await {
    //         let resp: serde_json::Value = serde_json::from_str(&msg.to_string()).unwrap();
    //         assert_eq!(resp[1], "ref1");
    //         assert_eq!(resp[2], "system");
    //         assert_eq!(resp[4]["status"], "ok");
    //     }
    //
    //     // Leave channel
    //     let leave_msg = r#"["1","ref2","system","phx_leave",{}]"#;
    //     tx.send(Message::text(leave_msg)).await.unwrap();
    //
    //     // Verify leave response
    //     if let Some(Ok(msg)) = rx.next().await {
    //         let resp: serde_json::Value = serde_json::from_str(&msg.to_string()).unwrap();
    //         assert_eq!(resp[1], "ref2");
    //         assert_eq!(resp[2], "system");
    //         assert_eq!(resp[4]["status"], "ok");
    //     }
    //
    //     // Verify channel state
    //     assert!(state
    //         .ctl
    //         .lock()
    //         .await
    //         .channel_map
    //         .lock()
    //         .await
    //         .get("system")
    //         .unwrap()
    //         .empty());
    // }

    #[tokio::test]
    async fn test_multiple_clients() {
        let (addr, state) = setup_test_server().await;

        // Connect multiple clients
        let mut clients = vec![];
        for i in 0..3 {
            let (mut tx, mut rx) = connect_client(&addr).await;

            // Join system channel
            let join_msg = format!(
                r#"["{}","ref{}","system","phx_join",{{"token":"test"}}]"#,
                i, i
            );
            tx.send(Message::text(join_msg)).await.unwrap();

            // Verify join
            if let Some(Ok(msg)) = rx.next().await {
                let resp: serde_json::Value = serde_json::from_str(&msg.to_string()).unwrap();
                assert_eq!(resp[4]["status"], "ok");
            }

            clients.push((tx, rx));
        }

        assert_eq!(
            state
                .ctl
                .lock()
                .await
                .channels
                .lock()
                .await
                .get("system")
                .unwrap()
                .count
                .load(std::sync::atomic::Ordering::SeqCst),
            3
        );
    }

    #[tokio::test]
    async fn test_message_broadcast() {
        let (addr, state) = setup_test_server().await;
        let (mut tx1, mut rx1) = connect_client(&addr).await;
        let (mut tx2, mut rx2) = connect_client(&addr).await;

        // Both clients join system channel
        for (tx, i) in [(&mut tx1, 1), (&mut tx2, 2)] {
            let join_msg = format!(
                r#"["{}","ref{}","system","phx_join",{{"token":"test"}}]"#,
                i, i
            );
            tx.send(Message::text(join_msg)).await.unwrap();

            // Wait for join response
            if let Some(Ok(_)) = if i == 1 {
                rx1.next().await
            } else {
                rx2.next().await
            } {}
        }

        // Broadcast message to system channel
        let message = ReplyMessage {
            join_reference: None,
            reference: "broadcast".to_string(),
            topic: "system".to_string(),
            event: "test".to_string(),
            payload: ReplyPayload {
                status: "ok".to_string(),
                response: Response::Message {
                    message: "test broadcast".to_string(),
                },
            },
        };

        state
            .ctl
            .lock()
            .await
            .channel_broadcast("system".to_string(), ChannelMessage::Reply(message))
            .await
            .unwrap();

        // Both clients should receive the message
        for rx in [&mut rx1, &mut rx2] {
            if let Some(Ok(msg)) = rx.next().await {
                let resp: serde_json::Value = serde_json::from_str(&msg.to_string()).unwrap();
                assert_eq!(resp[1], "broadcast");
                assert_eq!(resp[4]["response"]["message"], "test broadcast");
            }
        }
    }

    // FIXME: not cleaned up
    //
    // #[tokio::test]
    // async fn test_connection_close() {
    //     let (addr, state) = setup_test_server().await;
    //     let (mut tx, mut rx) = connect_client(&addr).await;
    //
    //     let join_msg = r#"["1","ref1","system","phx_join",{"token":"test"}]"#;
    //     tx.send(Message::text(join_msg)).await.unwrap();
    //     rx.next().await;
    //
    //     drop(tx);
    //     drop(rx);
    //
    //     tokio::time::sleep(Duration::from_millis(100)).await;
    //     assert!(state
    //         .ctl
    //         .lock()
    //         .await
    //         .channel_map
    //         .lock()
    //         .await
    //         .get("system")
    //         .unwrap()
    //         .empty());
    // }

    #[tokio::test]
    async fn test_invalid_messages() {
        let (addr, _) = setup_test_server().await;
        let (mut tx, mut rx) = connect_client(&addr).await;

        // Send invalid JSON
        tx.send(Message::text("invalid json")).await.unwrap();

        // Send invalid message format
        tx.send(Message::text(r#"["invalid","format"]"#))
            .await
            .unwrap();

        // Send to non-existent channel
        let invalid_channel = r#"["1","ref1","nonexistent","phx_join",{"token":"test"}]"#;
        tx.send(Message::text(invalid_channel)).await.unwrap();

        // Connection should still be alive
        let heartbeat = r#"[null,"1","phoenix","heartbeat",{}]"#;
        tx.send(Message::text(heartbeat)).await.unwrap();

        if let Some(Ok(msg)) = rx.next().await {
            let resp: serde_json::Value = serde_json::from_str(&msg.to_string()).unwrap();
            assert_eq!(resp[2], "phoenix");
            assert_eq!(resp[4]["status"], "ok");
        }
    }

    #[tokio::test]
    async fn test_system_channel() {
        let (addr, _) = setup_test_server().await;
        let (mut tx, mut rx) = connect_client(&addr).await;

        // Join system channel
        let join_msg = r#"["1","ref1","system","phx_join",{"token":"test"}]"#;
        tx.send(Message::text(join_msg)).await.unwrap();

        // Should receive initial join response
        if let Some(Ok(msg)) = rx.next().await {
            let resp: serde_json::Value = serde_json::from_str(&msg.to_string()).unwrap();
            assert_eq!(resp[2], "system");
            assert_eq!(resp[4]["status"], "ok");
        }

        // Should receive datetime updates
        if let Some(Ok(msg)) = rx.next().await {
            let resp: serde_json::Value = serde_json::from_str(&msg.to_string()).unwrap();
            assert_eq!(resp[2], "system");
            assert!(resp[4]["response"]["datetime"].is_string());
        }
    }

    #[test]
    fn test_response_deserialize() {
        // Empty with null type
        let json = r#"{"type": "null"}"#;
        let response: Response = serde_json::from_str(json).unwrap();
        assert!(matches!(response, Response::Empty {}));

        // Join type
        let json = r#"{"type": "join"}"#;
        let response: Response = serde_json::from_str(json).unwrap();
        assert!(matches!(response, Response::Join {}));

        // Heartbeat type
        let json = r#"{"type": "heartbeat"}"#;
        let response: Response = serde_json::from_str(json).unwrap();
        assert!(matches!(response, Response::Heartbeat {}));

        // Message with payload
        let json = r#"{"type": "message", "message": "hello world"}"#;
        let response: Response = serde_json::from_str(json).unwrap();
        assert!(matches!(response, Response::Message { message } if message == "hello world"));

        // Datetime with fields
        let json = r#"{"type": "datetime", "datetime": "2024-01-01", "counter": 42}"#;
        let response: Response = serde_json::from_str(json).unwrap();
        assert!(matches!(response, Response::Datetime { datetime, counter } 
            if datetime == "2024-01-01" && counter == 42));
    }

    #[test]
    fn test_response_serialize() {
        // Empty serializes with null type
        let response = Response::Empty {};
        assert_eq!(
            serde_json::to_string(&response).unwrap(),
            r#"{"type":"null"}"#
        );

        // Join with join type
        let response = Response::Join {};
        assert_eq!(
            serde_json::to_string(&response).unwrap(),
            r#"{"type":"join"}"#
        );

        // Heartbeat with join type
        let response = Response::Heartbeat {};
        assert_eq!(
            serde_json::to_string(&response).unwrap(),
            r#"{"type":"heartbeat"}"#
        );

        // Message includes payload
        let response = Response::Message {
            message: "hello".to_string(),
        };
        assert_eq!(
            serde_json::to_string(&response).unwrap(),
            r#"{"type":"message","message":"hello"}"#
        );

        // Datetime includes all fields
        let response = Response::Datetime {
            datetime: "2024-01-01".to_string(),
            counter: 42,
        };
        assert_eq!(
            serde_json::to_string(&response).unwrap(),
            r#"{"type":"datetime","datetime":"2024-01-01","counter":42}"#
        );
    }

    #[test]
    fn test_response_invalid_json() {
        // Missing type field
        let json = r#"{"message": "hello"}"#;
        assert!(serde_json::from_str::<Response>(json).is_err());

        // Invalid type value
        let json = r#"{"type": "invalid"}"#;
        assert!(serde_json::from_str::<Response>(json).is_err());

        // Missing required fields
        let json = r#"{"type": "datetime", "datetime": "2024-01-01"}"#;
        assert!(serde_json::from_str::<Response>(json).is_err());
    }
}
