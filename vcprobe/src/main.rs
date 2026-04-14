mod display;
mod nats_transport;
mod proctor;
mod state;
mod transport;

use std::process;

use anyhow::Result;
use clap::Parser;
use protobuf::Message;
use tokio::sync::mpsc;
use tokio::time::Duration;
use url::Url;
use videocall_types::protos::packet_wrapper::packet_wrapper::PacketType;
use videocall_types::protos::packet_wrapper::PacketWrapper;

#[derive(Parser)]
#[command(
    name = "vcprobe",
    about = "videocall.rs meeting diagnostic probe",
    long_about = "Join a videocall.rs meeting as a passive observer and print human-readable packet summaries.\n\nURL format:\n  https://<host>/lobby/<username>/<meeting_id>  (WebTransport)\n  wss://<host>/lobby/<username>/<meeting_id>    (WebSocket)\n\nNATS mode:\n  vcprobe --nats <url> --meeting <id> [--proctor]"
)]
struct Args {
    /// Meeting URL — https://<host>/lobby/<username>/<meeting_id>  or  wss://...
    /// (not required with --nats)
    #[arg(conflicts_with = "nats_url")]
    url: Option<String>,

    /// NATS server URL (e.g., nats://localhost:4222)
    #[arg(long = "nats", value_name = "URL")]
    nats_url: Option<String>,

    /// Meeting ID to monitor (required with --nats)
    #[arg(long = "meeting", value_name = "ID", requires = "nats_url")]
    meeting_id: Option<String>,

    /// Full-screen live dashboard: participant list, video/mic/talking status, call quality
    #[arg(long)]
    proctor: bool,

    /// Verbose output (show sequence numbers, sizes, crypto packets)
    #[arg(short = 'v', long = "verbose")]
    verbose: bool,

    /// Skip TLS certificate verification
    #[arg(long)]
    insecure: bool,

    /// Join handshake then exit immediately (connectivity health check)
    #[arg(long)]
    probe: bool,

    /// Exit after N seconds of observation
    #[arg(long, value_name = "N")]
    timeout: Option<u64>,

    /// Suppress all status lines; only show packet summary lines
    #[arg(short, long)]
    quiet: bool,

    /// Display timestamps in UTC instead of local time
    #[arg(long)]
    utc: bool,
}

struct ParsedUrl {
    url: Url,
    username: String,
    meeting_id: String,
    use_websocket: bool,
}

fn parse_meeting_url(raw: &str) -> Result<ParsedUrl> {
    let url = Url::parse(raw).map_err(|e| {
        anyhow::anyhow!(
            "invalid URL — expected: https://<host>/lobby/<username>/<meeting_id>\n         got: {} ({})",
            raw, e
        )
    })?;

    let scheme = url.scheme();
    let use_websocket = match scheme {
        "https" | "wt" => false,
        "wss" | "ws" => true,
        other => anyhow::bail!(
            "invalid URL — scheme must be https, wt, wss, or ws\n         got: {}://",
            other
        ),
    };

    let path = url.path().to_string();
    let segs: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();

    if segs.len() < 3 || segs[0] != "lobby" {
        anyhow::bail!(
            "invalid URL — expected: {}://<host>/lobby/<username>/<meeting_id>\n         got: {}",
            scheme,
            raw
        );
    }

    Ok(ParsedUrl {
        url,
        username: segs[1].to_string(),
        meeting_id: segs[2].to_string(),
        use_websocket,
    })
}

async fn connect(parsed: &ParsedUrl, insecure: bool) -> Result<mpsc::Receiver<Vec<u8>>> {
    if parsed.use_websocket {
        transport::connect_websocket(
            parsed.url.clone(),
            insecure,
            parsed.username.clone(),
            parsed.meeting_id.clone(),
        )
        .await
    } else {
        transport::connect_webtransport(
            parsed.url.clone(),
            insecure,
            parsed.username.clone(),
            parsed.meeting_id.clone(),
        )
        .await
    }
}

async fn wait_for_session(rx: &mut mpsc::Receiver<Vec<u8>>) -> Option<u64> {
    while let Some(raw) = rx.recv().await {
        if let Ok(pkt) = PacketWrapper::parse_from_bytes(&raw) {
            if pkt.packet_type.enum_value() == Ok(PacketType::SESSION_ASSIGNED) {
                return Some(pkt.session_id);
            }
        }
    }
    None
}

#[tokio::main]
async fn main() {
    let args = Args::parse();

    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();

    // ── Determine mode: NATS or WebTransport/WebSocket ───────────────────────
    let (mut rx, meeting_id, transport_name) = if let Some(nats_url) = args.nats_url {
        // NATS mode
        let meeting_id = match args.meeting_id.as_ref() {
            Some(id) => id,
            None => {
                eprintln!("error: --meeting <ID> is required when using --nats");
                process::exit(2);
            }
        };

        // Validate --probe not used with --nats
        if args.probe {
            eprintln!("error: --probe mode not supported with --nats (no session to establish)");
            process::exit(2);
        }

        if !args.quiet && !args.proctor {
            eprintln!("* connecting to NATS: {}", nats_url);
            eprintln!("* subscribing to meeting: {}", meeting_id);
        }

        let rx = match transport::connect_nats(nats_url.clone(), meeting_id.clone()).await {
            Ok(r) => r,
            Err(e) => {
                eprintln!("* NATS connection failed: {}", e);
                process::exit(1);
            }
        };

        (rx, meeting_id.clone(), "NATS")
    } else {
        // Existing WebTransport/WebSocket mode
        let url_str = match args.url.as_ref() {
            Some(u) => u,
            None => {
                eprintln!("error: URL argument is required (or use --nats mode)");
                eprintln!("Try 'vcprobe --help' for more information.");
                process::exit(2);
            }
        };
        let parsed = match parse_meeting_url(url_str) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("error: {}", e);
                process::exit(2);
            }
        };

        let transport_name = if parsed.use_websocket {
            "WebSocket"
        } else {
            "WebTransport"
        };

        if !args.quiet && !args.proctor {
            eprintln!("* connecting to {} [{}]", url_str, transport_name);
        }

        let mut rx = match connect(&parsed, args.insecure).await {
            Ok(r) => r,
            Err(e) => {
                eprintln!("* connection failed: {}", e);
                process::exit(1);
            }
        };

        // ── Probe mode (WebTransport/WebSocket only) ──────────────────────────
        if args.probe {
            let probe_timeout_secs = args.timeout.unwrap_or(5);
            let result = tokio::time::timeout(
                Duration::from_secs(probe_timeout_secs),
                wait_for_session(&mut rx),
            )
            .await;

            match result {
                Ok(Some(session_id)) => {
                    if !args.quiet {
                        eprintln!("* connected to {} [{}]", url_str, transport_name);
                        eprintln!(
                            "* joined meeting \"{}\" as \"{}\" (session={})",
                            parsed.meeting_id, parsed.username, session_id
                        );
                        eprintln!("* probe complete");
                    }
                    process::exit(0);
                }
                Ok(None) => {
                    eprintln!("* probe failed: connection closed before session assignment");
                    process::exit(1);
                }
                Err(_) => {
                    eprintln!(
                        "* probe failed: timeout after {}s waiting for session assignment",
                        probe_timeout_secs
                    );
                    process::exit(1);
                }
            }
        }

        (rx, parsed.meeting_id, transport_name)
    };

    // ── Proctor mode (full-screen TUI) ────────────────────────────────────────
    if args.proctor {
        if let Err(e) = proctor::run(rx, meeting_id.clone(), args.utc).await {
            eprintln!("proctor error: {}", e);
            process::exit(1);
        }
        return;
    }

    // ── Observation mode (scrolling log) ─────────────────────────────────────
    if !args.quiet {
        eprintln!(
            "* observing meeting \"{}\" [{}] — Ctrl-C to stop",
            meeting_id, transport_name
        );
    }

    let obs = async {
        while let Some(raw) = rx.recv().await {
            display::handle_packet(&raw, args.verbose, args.utc);
        }
        if !args.quiet {
            eprintln!("* connection closed");
        }
    };

    if let Some(secs) = args.timeout {
        let _ = tokio::time::timeout(Duration::from_secs(secs), obs).await;
    } else {
        obs.await;
    }
}
