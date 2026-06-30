@0xa1b2c3d4e5f60003;

struct NetEqOperationCounters {
  normalPerSec @0 :Float64;
  expandPerSec @1 :Float64;
  acceleratePerSec @2 :Float64;
  fastAcceleratePerSec @3 :Float64;
  preemptiveExpandPerSec @4 :Float64;
  mergePerSec @5 :Float64;
  comfortNoisePerSec @6 :Float64;
  dtmfPerSec @7 :Float64;
  undefinedPerSec @8 :Float64;
}

struct NetEqNetwork {
  operationCounters @0 :NetEqOperationCounters;
}

struct NetEqStats {
  currentBufferSizeMs @0 :Float64;
  packetsAwaitingDecode @1 :Float64;
  network @2 :NetEqNetwork;
  packetsPerSec @3 :Float64;
}

struct VideoStats {
  fpsReceived @0 :Float64;
  framesBuffered @1 :Float64;
  framesDecoded @2 :UInt64;
  bitrateKbps @3 :UInt64;
}

struct PeerStats {
  canListen @0 :Bool;
  canSee @1 :Bool;
  audioEnabled @2 :Bool;
  videoEnabled @3 :Bool;
  neteqStats @4 :NetEqStats;
  videoStats @5 :VideoStats;
}

struct PeerStatsEntry {
  key @0 :Text;
  value @1 :PeerStats;
}

struct HealthPacket {
  sessionId @0 :Text;
  meetingId @1 :Text;
  reportingPeer @2 :Text;
  timestampMs @3 :UInt64;
  reportingAudioEnabled @4 :Bool;
  reportingVideoEnabled @5 :Bool;
  peerStats @6 :List(PeerStatsEntry);
  activeServerUrl @7 :Text;
  activeServerType @8 :Text;
  activeServerRttMs @9 :Float64;
}
