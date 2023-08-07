use chrono::Utc;
use futures::stream::FuturesUnordered;
use futures::SinkExt;
use futures::StreamExt;
use protobuf::Message as ProtoMessage;
use rand::Rng;
use std::env;
use tokio::task::JoinHandle;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use types::protos::media_packet::media_packet::MediaType;
use types::protos::media_packet::MediaPacket;
use url::Url;

#[tokio::main]
async fn main() {
    dotenv::dotenv().ok();

    let n_clients = env::var("N_CLIENTS").unwrap().parse::<usize>().unwrap();
    let endpoint = env::var("ENDPOINT").unwrap();
    let room = env::var("ROOM").unwrap();
    let echo_user = env::var("ECHO_USER").unwrap();
    let email_prefix = env::var("EMAIL_PREFIX").unwrap_or_else(|_| "".to_string());

    (0..n_clients)
        .map(|_| async {
            let handle = create_client(&endpoint, &room, &echo_user, &email_prefix).await;
            let _ = handle.await;
        })
        .collect::<FuturesUnordered<_>>()
        .collect::<Vec<_>>()
        .await;
}

async fn create_client(
    endpoint: &str,
    room: &str,
    echo_user: &str,
    email_prefix: &str,
) -> JoinHandle<()> {
    let email = generate_email(email_prefix);
    let url = format!("{}/lobby/{}/{}", endpoint, email, room);
    let (mut ws_stream, _) = connect_async(Url::parse(&url).unwrap()).await.unwrap();
    println!("Connected to {}", url);
    let echo_user = echo_user.to_string();
    // Send a single heartbeat just so that we show up on the ui
    let mut media_packet = MediaPacket::default();
    media_packet.media_type = MediaType::HEARTBEAT.into();
    media_packet.email = email.clone();
    media_packet.timestamp = Utc::now().timestamp_millis() as f64;
    let mut buf = Vec::new();
    media_packet.write_to_vec(&mut buf).unwrap();
    ws_stream.send(Message::Binary(buf)).await.unwrap();
    tokio::spawn(async move {
        let mut ws_stream = ws_stream;
        while let Some(msg) = ws_stream.next().await {
            let msg = msg.unwrap();
            match msg {
                Message::Text(text) => {
                    if text == "Hello" {
                        ws_stream.send("Hello".into()).await.unwrap();
                    }
                }
                Message::Binary(bin) => {
                    // decode bin as protobuf
                    let mut media_packet =
                        MediaPacket::parse_from_bytes(&bin.into_boxed_slice()).unwrap();

                    // rewrite whatever is in the protobuf so that it seems like it is coming from this bot
                    if media_packet.email == echo_user {
                        media_packet.email = email.clone();

                        // send the protobuf back to the server
                        let mut buf = Vec::new();
                        media_packet.write_to_vec(&mut buf).unwrap();
                        ws_stream.send(Message::Binary(buf)).await.unwrap();
                    }
                }
                Message::Ping(data) => {
                    ws_stream.send(Message::Pong(data)).await.unwrap();
                }
                _ => {}
            }
        }
    })
}

fn generate_email(email_prefix: &str) -> String {
    const CHARSET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";

    let mut rng = rand::thread_rng();
    let email: String = (0..10)
        .map(|_| {
            let idx = rng.gen_range(0..CHARSET.len());
            CHARSET[idx] as char
        })
        .collect();

    format!("{}{}@example.com", email_prefix, email)
}
