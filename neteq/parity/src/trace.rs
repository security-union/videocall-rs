//! Serializable packet traces.
//!
//! A trace is the sender-side packet sequence plus each packet's arrival
//! time at the receiver. JSON on disk; payloads are hex. With
//! `--no-payloads` only `payload_len` is stored and the payloads are
//! regenerated from `(scenario, codec, seed, duration_ms)` on load, since the
//! generators are deterministic.

use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::{HarnessError, Result};
use crate::types::{Codec, NetEqConfig, RtpHeader};

/// Bump when generators or the format change in a way that alters traces.
pub const TRACE_FORMAT_VERSION: u32 = 1;

/// One RTP packet as seen by the receiver.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TracePacket {
    /// RTP sequence number.
    pub seq: u16,
    /// RTP timestamp.
    pub rtp_timestamp: u32,
    /// Receiver arrival time, ms since the start of the simulation.
    pub arrival_ms: i64,
    /// RTP payload type.
    pub payload_type: u8,
    /// RTP marker bit.
    pub marker: bool,
    /// Payload length in bytes (always present).
    pub payload_len: usize,
    /// Payload bytes (hex on disk); empty when stored without payloads.
    #[serde(default, with = "hex_bytes", skip_serializing_if = "Vec::is_empty")]
    pub payload: Vec<u8>,
}

impl TracePacket {
    /// RTP header for this packet.
    pub fn header(&self, ssrc: u32) -> RtpHeader {
        RtpHeader {
            payload_type: self.payload_type,
            marker: self.marker,
            sequence_number: self.seq,
            timestamp: self.rtp_timestamp,
            ssrc,
        }
    }
}

/// A complete, self-describing trace.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Trace {
    /// [`TRACE_FORMAT_VERSION`] at generation time.
    pub version: u32,
    /// Scenario name (see [`crate::scenarios::Scenario`]).
    pub scenario: String,
    /// Generator seed.
    pub seed: u64,
    /// Codec of every media packet.
    pub codec: Codec,
    /// Packet duration in ms.
    pub ptime_ms: u32,
    /// Nominal media duration in ms.
    pub duration_ms: u32,
    /// RTP SSRC.
    pub ssrc: u32,
    /// Whether RFC 3389 CN packets are present (CN decoder must be registered).
    pub with_cn: bool,
    /// NetEq configuration both implementations are created with.
    pub config: NetEqConfig,
    /// How payloads were produced, for humans.
    pub payload_generator: String,
    /// Packets in send order.
    pub packets: Vec<TracePacket>,
}

impl Trace {
    /// True when every packet carries its payload bytes.
    pub fn has_payloads(&self) -> bool {
        self.packets
            .iter()
            .all(|p| p.payload.len() == p.payload_len)
    }

    /// Indices of packets in receiver arrival order (stable for ties).
    pub fn arrival_order(&self) -> Vec<usize> {
        let mut idx: Vec<usize> = (0..self.packets.len()).collect();
        idx.sort_by_key(|&i| (self.packets[i].arrival_ms, i));
        idx
    }

    /// Last arrival time, or 0 for an empty trace.
    pub fn last_arrival_ms(&self) -> i64 {
        self.packets.iter().map(|p| p.arrival_ms).max().unwrap_or(0)
    }

    /// Writes JSON; `with_payloads = false` strips payload bytes.
    pub fn save(&self, path: &Path, with_payloads: bool) -> Result<()> {
        let json = if with_payloads {
            serde_json::to_string(self)?
        } else {
            let mut stripped = self.clone();
            for p in &mut stripped.packets {
                p.payload.clear();
            }
            serde_json::to_string(&stripped)?
        };
        fs::write(path, json)?;
        Ok(())
    }

    /// Reads JSON, regenerating payloads if they were stripped.
    pub fn load(path: &Path) -> Result<Trace> {
        let mut trace: Trace = serde_json::from_str(&fs::read_to_string(path)?)?;
        if !trace.has_payloads() {
            trace.rehydrate()?;
        }
        Ok(trace)
    }

    /// Regenerates payload bytes from the scenario generator and checks the
    /// result matches the stored packet metadata.
    pub fn rehydrate(&mut self) -> Result<()> {
        let scenario = crate::scenarios::Scenario::from_name(&self.scenario)?;
        let fresh = scenario.generate(self.codec, self.seed, self.duration_ms)?;
        let consistent = fresh.packets.len() == self.packets.len()
            && fresh.packets.iter().zip(&self.packets).all(|(a, b)| {
                a.seq == b.seq
                    && a.rtp_timestamp == b.rtp_timestamp
                    && a.payload_len == b.payload_len
            });
        if !consistent || fresh.version != self.version {
            return Err(HarnessError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "trace metadata does not match its generator; it needs stored payloads",
            )));
        }
        for (dst, src) in self.packets.iter_mut().zip(fresh.packets) {
            dst.payload = src.payload;
        }
        Ok(())
    }
}

mod hex_bytes {
    use serde::{de::Error, Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(bytes: &[u8], s: S) -> Result<S::Ok, S::Error> {
        let mut out = String::with_capacity(bytes.len() * 2);
        for b in bytes {
            out.push_str(&format!("{b:02x}"));
        }
        s.serialize_str(&out)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let s = String::deserialize(d)?;
        if s.len() % 2 != 0 {
            return Err(D::Error::custom("odd-length hex payload"));
        }
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(D::Error::custom))
            .collect()
    }
}
