//! Audio codecs NetEq decodes: PCM16B, Opus, and RFC 3389 comfort noise.

pub mod builtin_factory;
pub mod cng;
#[cfg(feature = "ropus")]
pub mod opus;
pub mod pcm16b;

pub use builtin_factory::BuiltinAudioDecoderFactory;
