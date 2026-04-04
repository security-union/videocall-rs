@0xa1b2c3d4e5f60001;

enum MediaType {
  unknown @0;
  video @1;
  audio @2;
  screen @3;
  heartbeat @4;
  rtt @5;
}

enum VideoCodec {
  unspecified @0;
  vp8 @1;
  vp9Profile0Level108bit @2;
}

struct AudioMetadata {
  audioFormat @0 :Text;
  audioNumberOfChannels @1 :UInt32;
  audioNumberOfFrames @2 :UInt32;
  audioSampleRate @3 :Float32;
  sequence @4 :UInt64;
}

struct VideoMetadata {
  sequence @0 :UInt64;
  codec @1 :VideoCodec;
}

struct HeartbeatMetadata {
  videoEnabled @0 :Bool;
  audioEnabled @1 :Bool;
  screenEnabled @2 :Bool;
}

struct MediaPacket {
  mediaType @0 :MediaType;
  email @1 :Text;
  data @2 :Data;
  frameType @3 :Text;
  timestamp @4 :Float64;
  duration @5 :Float64;
  audioMetadata @6 :AudioMetadata;
  videoMetadata @7 :VideoMetadata;
  heartbeatMetadata @8 :HeartbeatMetadata;
}
