use futures::SinkExt;
use rand::Rng;
use std::env;
use tokio::{net::TcpStream, task::JoinHandle, try_join};
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};
use url::Url;

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
        println!("Spawned");
        handle_connection(ws_stream).await;
    })
}

async fn handle_connection(mut ws_stream: WebSocketStream<MaybeTlsStream<TcpStream>>) {
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
