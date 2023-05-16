use futures::SinkExt;
use futures::StreamExt;
use rand::Rng;
use types::protos::media_packet::MediaPacket;
use std::env;
use tokio::{net::TcpStream, task::JoinHandle};
use tokio_tungstenite::{connect_async, tungstenite::Message, MaybeTlsStream, WebSocketStream};
use url::Url;
use protobuf::Message as ProtoMessage;

#[tokio::main]
async fn main() {
    let n_clients = env::var("N_CLIENTS").unwrap().parse::<usize>().unwrap();
    let endpoint = env::var("ENDPOINT").unwrap();
    let room = env::var("ROOM").unwrap();

    // create n_clients and await for them to be created.
    let mut clients = Vec::new();
    for _ in 0..n_clients {
        clients.push(create_client(&endpoint, &room).await);
    }

    for client in clients {
        client.await;
    }
}

async fn create_client(endpoint: &str, room: &str) -> JoinHandle<()> {
    let email = generate_email();
    let url = format!("{}/lobby/{}/{}", endpoint, email, room);
    let (ws_stream, _) = connect_async(Url::parse(&url).unwrap()).await.unwrap();
    println!("Connected to {}", url);
    tokio::spawn(async move {
        let mut ws_stream = ws_stream;
        while let Some(msg) = ws_stream.next().await {
            let msg = msg.unwrap();
            match msg {
                Message::Text(text) => {
                    if text == "Hello" {
                        ws_stream.send("Hello".into()).await.unwrap();
                    }
                },
                Message::Binary(bin) => {
                    // decode bin as protobuf
                    let mut media_packet = MediaPacket::parse_from_bytes(&bin.into_boxed_slice()).unwrap();
                    
                    // rewrite whatever is in the protobuf so that it seems like it is coming from this bot
                    media_packet.email = email.clone();

                    // send the protobuf back to the server
                    let mut buf = Vec::new();
                    media_packet.write_to_vec(&mut buf).unwrap();
                    ws_stream.send(Message::Binary(buf)).await.unwrap();
                }
                Message::Ping(data) => {
                    ws_stream.send(Message::Pong(data)).await.unwrap();
                }
                _ => {}
            }
        }
    })
}

async fn send_hello(mut ws_stream: WebSocketStream<MaybeTlsStream<TcpStream>>) {
    println!("Connected");
    loop {
        ws_stream.send("Hello".into()).await.unwrap();
        // ws_stream.next().await;
        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
    }
    println!("Disconnected");
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
