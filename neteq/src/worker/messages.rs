/*
 * Copyright 2025 Security Union LLC
 *
 * Licensed under either of
 *
 * * Apache License, Version 2.0
 *   (http://www.apache.org/licenses/LICENSE-2.0)
 * * MIT license
 *   (http://opensource.org/licenses/MIT)
 *
 * at your option.
 *
 * Unless you explicitly state otherwise, any contribution intentionally
 * submitted for inclusion in the work by you, as defined in the Apache-2.0
 * license, shall be dual licensed as above, without any additional terms or
 * conditions.
 */

//! Message types for neteq worker communication

use serde::{Deserialize, Serialize};
use wasm_bindgen::JsValue;

/// Messages sent from main thread to worker
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "camelCase")]
pub enum WorkerMsg {
    /// Insert an encoded packet
    Insert {
        seq: u16,
        timestamp: u32,
        #[serde(with = "serde_bytes")]
        payload: Vec<u8>,
    },
    Flush,
    Clear,
    Close,
    /// Mute/unmute audio output
    Mute {
        muted: bool,
    },
    /// Enable/disable diagnostics reporting
    SetDiagnostics {
        enabled: bool,
    },
}

/// Messages sent from worker back to main thread
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum WorkerResponse {
    WorkerReady {
        mute_state: bool,
    },
    Stats {
        #[serde(skip_serializing, skip_deserializing)]
        stats: JsValue,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use wasm_bindgen_test::*;

    #[wasm_bindgen_test]
    fn test_mute_message_serialization() {
        let msg = WorkerMsg::Mute { muted: true };
        let serialized = serde_wasm_bindgen::to_value(&msg).unwrap();
        let deserialized: WorkerMsg = serde_wasm_bindgen::from_value(serialized).unwrap();

        match deserialized {
            WorkerMsg::Mute { muted } => assert!(muted),
            _ => panic!("Wrong variant"),
        }
    }

    #[wasm_bindgen_test]
    fn test_insert_message_serialization() {
        let msg = WorkerMsg::Insert {
            seq: 123,
            timestamp: 456,
            payload: vec![1, 2, 3],
        };
        let serialized = serde_wasm_bindgen::to_value(&msg).unwrap();
        let deserialized: WorkerMsg = serde_wasm_bindgen::from_value(serialized).unwrap();

        match deserialized {
            WorkerMsg::Insert {
                seq,
                timestamp,
                payload,
            } => {
                assert_eq!(seq, 123);
                assert_eq!(timestamp, 456);
                assert_eq!(payload, vec![1, 2, 3]);
            }
            _ => panic!("Wrong variant"),
        }
    }
}
