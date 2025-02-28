use futures::StreamExt;
use protobuf::Message;
use quinn::Endpoint;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use url::Url;
use videocall_types::protos::{media_packet::{media_packet::MediaType, MediaPacket}, packet_wrapper::PacketWrapper};


#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut endpoint = quinn::Endpoint::client("[::]:0".parse()?)?;
    // endpoint.set_default_client_config(/* ... */);

    // Connect to the given URL.
    let url: Url = "https://transport.rustlemania.com/lobby/dariote/1234".parse()?;
    
    let client = web_transport_quinn::ClientBuilder::new().with_system_roots()?;

    // Connect to the given URL.
    let session: web_transport_quinn::Session = client.connect(&url).await?;

    // Process incoming streams
    let session = session.clone();
    let session_clone = session.clone();
    tokio::spawn(async move {
        while let Ok(mut stream) = session.accept_uni().await {
        // Spawn a new task to handle each stream
            tokio::spawn(async move {
                // Read stream to the end
                let data = stream.read_to_end(usize::MAX).await.unwrap();
                process_packet(&data);  
            });
        }
    });

    let result = tokio::spawn(async move {
        while let Ok(stream) = session_clone.read_datagram().await {
            process_packet(&stream);
        }
    });

    result.await;

    Ok(())
}

fn process_packet(data:&[u8]) {
    let packetWrapper = PacketWrapper::parse_from_bytes(&data).unwrap();
    let media_packet = MediaPacket::parse_from_bytes(&packetWrapper.data).unwrap();
    // If it is a video packet log the sequence number
    if media_packet.media_type.enum_value_or_default() == MediaType::VIDEO {
        println!("Received video packet with sequence number: {}", media_packet.video_metadata.sequence);
    }
}
