// Compiled only with feature `tools`.
//
// Port target: WebRTC modules/audio_coding/neteq/tools/{neteq_test,
// neteq_input, audio_sink}.{h,cc} at rev
// 6341cca49ffcad6fcb4603bd6cd1325fd0fe28bd, reduced to what the parity
// harness and the integration tests need (PARITY_PLAN.md 5.6, 7.1).

//! NetEq simulation driver shared by tests and the parity harness.

pub mod audio_sink;
pub mod neteq_input;
pub mod neteq_test;

pub use audio_sink::{AudioSink, VectorAudioSink, VoidAudioSink};
pub use neteq_input::{NetEqInput, PacketData, ScriptedInput, SetMinimumDelayInfo};
pub use neteq_test::{NetEqTest, SimulationStepResult};
