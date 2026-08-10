# Image roster. One file per image; `all` builds a farm of stream scripts for
# `make images`.
{ p, packages }:
let
  common = import ./common.nix { inherit p; };
  inherit (p) pkgsLinuxStatic;

  images = {
    websocket-server = import ./websocket-server.nix { inherit common packages; };
    webtransport-server = import ./webtransport-server.nix { inherit common packages; };
    metrics-server = import ./metrics-server.nix { inherit common packages; };
    server-stats = import ./server-stats.nix { inherit common packages; };
    meeting-api = import ./meeting-api.nix { inherit common packages pkgsLinuxStatic; };
    bot = import ./bot.nix { inherit common packages; };
    media-server = import ./media-server.nix { inherit common packages pkgsLinuxStatic; };
    dioxus-ui = import ./dioxus-ui.nix { inherit common packages p; };
    engineering-vlog = import ./engineering-vlog.nix { inherit common p; };
    website = import ./website.nix { inherit common packages p; };
  };
in
images // {
  # linkFarm of every stream script — `make images` iterates over it.
  all = p.pkgs.linkFarm "videocall-images"
    (p.pkgs.lib.mapAttrsToList (name: drv: { inherit name; path = drv; }) images);
}
