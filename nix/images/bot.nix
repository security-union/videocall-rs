# Synthetic-clients bot (headless WebTransport client).
# Compose service: synthetic-clients. Config via $BOT_CONFIG_PATH bind mount.
{ common, packages }:
common.mkServiceImage {
  name = "videocall/bot";
  config = {
    Cmd = [ "${packages.bot}/bin/bot" ];
  };
}
