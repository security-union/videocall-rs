import { waitForServices } from "./helpers/wait-for-services";

export default async function globalSetup() {
  // Server-independent specs (e.g. webcodecs-vp9-interop) can run without the
  // docker stack. Set E2E_SKIP_SERVICE_WAIT=1 to bypass the readiness probe.
  if (process.env.E2E_SKIP_SERVICE_WAIT) {
    console.log("E2E_SKIP_SERVICE_WAIT set — skipping service readiness wait");
    return;
  }
  await waitForServices();
}
