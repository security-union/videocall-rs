// Default runtime configuration. Developers may override individual keys
// locally by creating dioxus-ui/scripts/config.local.js (gitignored) — see
// config.local.js.example for a template. In production, this file is
// replaced wholesale by the Helm chart (helm/videocall-ui/templates/configmap-configjs.yaml).
window.__APP_CONFIG = ({
  apiBaseUrl: "http://localhost:8081",
  // No `meetingApiBaseUrl`: the wasm side falls back to `apiBaseUrl` when this
  // key is absent (see `dioxus-ui/src/constants.rs:14-16`). The legacy `:8082`
  // default that lived here from PR #726 (2026-05-11) until PR #909 was a
  // phantom port — nothing in the e2e stack listens on it, and prod overrides
  // this file wholesale via the Helm configmap-configjs template anyway.
  // Setting it to a wrong port silently broke the local stack any time the
  // file got reverted (e.g. after `git checkout` / `git stash pop`): trunk's
  // hot-reload watcher re-copies the host file into dist/ on every edit,
  // propagating the bad port back to the wasm.
  wsUrl: "ws://localhost:8080",
  webTransportHost: "https://127.0.0.1:4433",
  oauthEnabled: "false",
  e2eeEnabled: "false",
  webTransportEnabled: "true",
  firefoxEnabled: "false",
  usersAllowedToStream: "",
  serverElectionPeriodMs: 2000,
  audioBitrateKbps: 65,
  videoBitrateKbps: 100,
  screenBitrateKbps: 100,
  oauthProvider: "",
  vadThreshold: 0.02,
  oauthAuthUrl: "",
  oauthClientId: "",
  oauthRedirectUrl: "http://localhost:3001/auth/callback",
  oauthScopes: "openid email profile",
  oauthTokenUrl: "",
  oauthIssuer: "",
  oauthPrompt: "",
  oauthFlow: "",
  searchApiBaseUrl: "http://localhost:3000/api/search/v2",
  consoleLogUploadEnabled: "false",
  mockPeersEnabled: "false"
});