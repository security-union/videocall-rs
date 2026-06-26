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
  transportBadgeEnabled: "true",
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
  mockPeersEnabled: "false",
  // WASM logger max level. Valid values (case-insensitive): "trace", "debug",
  // "info", "warn", "error" (also "off"). Operators can change this WITHOUT a
  // code change to raise or lower client log verbosity.
  //
  // Interaction with consoleLogUploadEnabled:
  //   - Key PRESENT (any value, including "info"): that level is the CEILING,
  //     even while collecting — e.g. "info"/"warn" cut per-packet log volume;
  //     "trace" opts into the per-packet hot-path logs (emitted at trace!,
  //     otherwise off). This is how you reduce capture on a hot deployment.
  //   - Key ABSENT: when collection is ON the level bumps to "debug" (historical
  //     capture behaviour, so meeting analysis keeps working). To get that
  //     debug-on-collection default, OMIT this key — do not set it to "info".
  // The explicit "info" below caps local dev at info (collection is off here).
  logLevel: "info",
  // EXPERIMENTAL, TEST-ONLY: max simulcast layers a publisher may emit
  // (issue #989). 1 = feature OFF (single stream, identical to pre-simulcast).
  // Effective layers = min(this, device-capability ceiling).
  // WARNING: values > 1 have NO playback benefit yet — layers are not
  // tier-differentiated (PR B), receivers decode the base layer only, and the
  // relay does not filter layers. Raising it purely multiplies encode CPU and
  // uplink/relay egress. KEEP AT 1 IN PRODUCTION; raise to 2/3 only in
  // controlled test meetings (and the load-test bot) until relay per-receiver
  // layer selection lands.
  experimentalSimulcastMaxLayers: 1
});
