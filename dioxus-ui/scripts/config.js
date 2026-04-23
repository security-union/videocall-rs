// Default runtime configuration. Developers may override individual keys
// locally by creating dioxus-ui/scripts/config.local.js (gitignored) — see
// config.local.js.example for a template. In production, this file is
// replaced wholesale by the Helm chart (helm/videocall-ui/templates/configmap-configjs.yaml).
window.__APP_CONFIG = ({
  apiBaseUrl: "http://localhost:8081",
  meetingApiBaseUrl: "http://localhost:8082",
  wsUrl: "ws://localhost:8080",
  webTransportHost: "https://127.0.0.1:4433",
  oauthEnabled: "true",
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
