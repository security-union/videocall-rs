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
  showBuildGitInfo: "true",
  firefoxEnabled: "false",
  usersAllowedToStream: "",
  serverElectionPeriodMs: 2000,
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
  // Independent opt-out for health-packet hardware/network/battery telemetry.
  hardwareMetricsEnabled: "true",
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
  // Simulcast publisher ceiling (issues #989/#1082). This committed dev/E2E
  // fallback sets 1, which emits a single stream. Camera/screen effective layers
  // are min(this, device-capability ceiling); audio is min(this, its 3-rung
  // ladder). Values > 1 permit tier-differentiated rungs. Receivers choose one
  // rung per peer/kind and discard non-selected packets before decrypt/parse;
  // the relay's #989 layer filter never drops the base rung and filters
  // unselected upper rungs per receiver. Other earlier filters can still drop
  // media outright: #988 drops off-screen camera video at every layer, and
  // observer/waiting-room authorization drops MEDIA. With no recorded layer
  // preference the #989 filter fails open and forwards all published rungs.
  // Extra emitted rungs add encode CPU, publisher uplink, and relay egress.
  // Production Helm runtimeConfig omits this key, so
  // dioxus-ui/src/constants.rs resolves it to 3 (default ON), still bounded by
  // each publisher's applicable ceiling.
  experimentalSimulcastMaxLayers: 1,
  // Receiver load-test knobs (#2068/#2069; discussion #2066). Omit
  // maxReceivedLayer for no receive cap; a cap is representative only for an
  // explicitly named constrained/mobile-client population, never as a silent
  // default. skipCanvasPaint is non-representative: real visible tiles decode
  // and paint, while hidden tiles do neither. Skipping paint removes main-thread
  // work measured by the CPU-overload drift watchdog (discussion #562) and biases
  // capacity results optimistically; keep it false for representative runs.
  skipCanvasPaint: "false"
});
