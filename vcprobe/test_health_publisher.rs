// Test tool to publish mock HEALTH packets to NATS
// Usage: cargo run --bin test_health_publisher -- nats://localhost:4223 jay

use protobuf::Message;
use videocall_types::protos::health_packet::HealthPacket;
use videocall_types::to_user_id_bytes;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("Usage: {} <nats-url> <meeting-id>", args[0]);
        std::process::exit(1);
    }

    let nats_url = &args[1];
    let meeting_id = &args[2];

    println!("Connecting to NATS at {}", nats_url);
    let client = async_nats::connect(nats_url).await?;
    println!("Connected!");

    let topic = "health.diagnostics.us-east.websocket.test-server";
    println!(
        "Publishing mock HEALTH packets to {} every 5 seconds",
        topic
    );
    println!("Press Ctrl-C to stop\n");

    let mut counter = 0u64;
    loop {
        counter += 1;

        let reporting_user = format!("test-user-{}", counter % 2);
        let health_packet = HealthPacket {
            session_id: counter.to_string(),
            meeting_id: meeting_id.to_string(),
            reporting_user_id: to_user_id_bytes(&reporting_user),
            timestamp_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_millis() as u64,
            active_server_rtt_ms: 45.5 + (counter as f64 * 2.0),
            ..Default::default()
        };

        let bytes = health_packet.write_to_bytes()?;
        client.publish(topic, bytes.into()).await?;

        println!(
            "[{}] Published HEALTH packet: session={}, meeting={}, peer={}, rtt={:.1}ms",
            chrono::Local::now().format("%H:%M:%S"),
            health_packet.session_id,
            health_packet.meeting_id,
            reporting_user,
            health_packet.active_server_rtt_ms,
        );

        tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
    }
}
