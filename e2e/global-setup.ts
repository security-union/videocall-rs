import { waitForServices } from "./helpers/wait-for-services";

export default async function globalSetup() {
  await waitForServices();
}
