use futures::SinkExt;
use futures::StreamExt;
use protobuf::Message as ProtoMessage;
use rand::Rng;
use types::protos::media_packet::media_packet::MediaType;
use std::env;
use tokio::task::JoinHandle;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use types::protos::media_packet::MediaPacket;
use url::Url;

#[tokio::main]
async fn main() {
    let n_clients = env::var("N_CLIENTS").unwrap().parse::<usize>().unwrap();
    let endpoint = env::var("ENDPOINT").unwrap();
    let room = env::var("ROOM").unwrap();
    let echo_user = env::var("ECHO_USER").unwrap();

    // create n_clients and await for them to be created.
    let mut clients = Vec::new();
    for _ in 0..n_clients {
        clients.push(create_client(&endpoint, &room, &echo_user).await);
    }

    for client in clients {
        match client.await {
            Ok(_) => {}
            Err(e) => {
                eprintln!("Error joining bot handle: {}", e);
            }
        }
    }
}

async fn create_client(endpoint: &str, room: &str, echo_user: &str) -> JoinHandle<()> {
    let email = generate_email();
    let url = format!("{}/lobby/{}/{}", endpoint, email, room);
    let (ws_stream, _) = connect_async(Url::parse(&url).unwrap()).await.unwrap();
    println!("Connected to {}", url);
    let echo_user = echo_user.to_string();
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

                    // rewrite whatever video is in the protobuf so that it seems like it is coming from this bot
                    if media_packet.email == echo_user && media_packet.media_type.unwrap() == MediaType::VIDEO {
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

fn generate_email() -> String {
    const CHARSET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";

    let mut rng = rand::thread_rng();
    let email: String = (0..10)
        .map(|_| {
            let idx = rng.gen_range(0..CHARSET.len());
            CHARSET[idx] as char
        })
        .collect();

    format!("{}@example.com", email)
}
