@0xa1b2c3d4e5f60002;

enum PacketType {
  unknown @0;
  rsaPubKey @1;
  aesKey @2;
  media @3;
  connection @4;
  diagnostics @5;
  health @6;
  meeting @7;
  sessionAssigned @8;
}

struct PacketWrapper {
  packetType @0 :PacketType;
  email @1 :Text;
  data @2 :Data;
  sessionId @3 :UInt64;
}
