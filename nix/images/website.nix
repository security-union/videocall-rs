# Leptos marketing site: musl server binary + prerendered site assets.
# Docker Hub: securityunion/video-call-rs-website -> helm website charts.
{ common, packages, p }:
let
  inherit (p) pkgs;

  # /app layout matching the old image: /app/leptos_website + /app/site
  appRoot = pkgs.runCommand "website-app" { } ''
    mkdir -p $out/app
    cp ${packages.website}/bin/leptos_website $out/app/leptos_website
    cp -r ${packages.website}/site $out/app/site
  '';
in
common.mkServiceImage {
  name = "videocall/website";
  contents = [ appRoot ];
  config = {
    Cmd = [ "/app/leptos_website" ];
    WorkingDir = "/app";
    Env = [
      "RUST_LOG=info"
      "LEPTOS_SITE_ADDR=0.0.0.0:8080"
      "LEPTOS_SITE_ROOT=site"
    ];
    ExposedPorts."8080/tcp" = { };
  };
}
