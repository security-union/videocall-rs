import { tmpdir } from "node:os";
import { join } from "node:path";

import { describe, expect, it } from "vitest";

import { NETEM_IFB_DEV, NetemExecError, type NetemExec } from "./control/netem";
import { runBotsToCompletion } from "./orchestrator";

/** A pod's eth0 + ifb0 tc/ip state, answering each command after a tick so requests can interleave. */
function podKernel(): { exec: NetemExec; state: () => Record<string, unknown> } {
  let ethNetem = false;
  let hook = false;
  let redirect: string | null = null;
  let ifb = false;
  let ifbNetem = false;
  const fail = (msg: string): never => {
    throw new NetemExecError(msg, 2);
  };
  const exec: NetemExec = async (file, args) => {
    await new Promise((r) => setTimeout(r, 2));
    const argv = file === "tc" ? args : args.slice(args.indexOf("ip") + 1);
    const cmd = argv.join(" ");
    const tc = file === "tc";
    if (tc && cmd.startsWith("qdisc replace dev eth0 root netem")) ethNetem = true;
    else if (tc && cmd.startsWith(`qdisc replace dev ${NETEM_IFB_DEV} root netem`)) {
      if (!ifb) fail("Cannot find device");
      ifbNetem = true;
    } else if (tc && cmd === "qdisc del dev eth0 root") {
      if (!ethNetem) fail("Cannot delete qdisc with handle of zero");
      ethNetem = false;
    } else if (tc && cmd === `qdisc del dev ${NETEM_IFB_DEV} root`) {
      if (!ifbNetem) fail("Cannot delete qdisc with handle of zero");
      ifbNetem = false;
    } else if (tc && cmd === "qdisc del dev eth0 ingress") {
      if (!hook) fail("Cannot find specified qdisc on specified device");
      hook = false;
      redirect = null;
    } else if (tc && cmd === "qdisc add dev eth0 handle ffff: ingress") {
      if (hook) fail("File exists");
      hook = true;
    } else if (tc && cmd.startsWith("filter add dev eth0 parent ffff:")) {
      if (!hook || !ifb) fail("Cannot find device");
      redirect = NETEM_IFB_DEV;
    } else if (tc && cmd === "qdisc show dev eth0") {
      const lines = [ethNetem ? "qdisc netem 8001: root" : "qdisc noqueue 0: root"];
      if (hook) lines.push("qdisc ingress ffff: parent ffff:fff1");
      return { stdout: lines.join("\n"), stderr: "" };
    } else if (tc && cmd === `qdisc show dev ${NETEM_IFB_DEV}`) {
      if (!ifb) fail("Cannot find device");
      return { stdout: ifbNetem ? "qdisc netem 8002: root" : "", stderr: "" };
    } else if (!tc && cmd === `link show ${NETEM_IFB_DEV}`) {
      if (!ifb) fail("Device does not exist");
    } else if (!tc && cmd === `link add ${NETEM_IFB_DEV} type ifb`) {
      if (ifb) fail("File exists");
      ifb = true;
    } else if (!tc && cmd.startsWith(`link set ${NETEM_IFB_DEV}`)) {
      if (!ifb) fail("Cannot find device");
    } else if (!tc && cmd === `link del ${NETEM_IFB_DEV}`) {
      if (!ifb) fail("Cannot find device");
      ifb = false;
      ifbNetem = false;
    } else {
      throw new Error(`unmodelled command: ${file} ${cmd}`);
    }
    return { stdout: "", stderr: "" };
  };
  return {
    exec,
    state: () => ({ ethNetem, hook, redirectTarget: redirect && (ifb ? redirect : "*"), ifbNetem }),
  };
}

async function startPod(exec: NetemExec): Promise<{
  netem: (method: string, body?: unknown) => Promise<Response>;
  stop: () => Promise<void>;
}> {
  const token = "test-token";
  let port = 0;
  let listening!: () => void;
  const listened = new Promise<void>((r) => {
    listening = r;
  });
  const run = runBotsToCompletion({
    tasks: [],
    control: {
      port: 0,
      token,
      tokenFilePath: join(tmpdir(), "bots-app-netem-test-ctl.json"),
      netem: { iface: "eth0", exec },
      onListen: async (info) => {
        port = info.port;
        listening();
      },
    },
  });
  await listened;
  return {
    netem: (method, body) =>
      fetch(`http://127.0.0.1:${port}/netem`, {
        method,
        headers: { authorization: `Bearer ${token}`, "content-type": "application/json" },
        body: body === undefined ? undefined : JSON.stringify(body),
      }),
    stop: async () => {
      process.emit("SIGTERM");
      await run;
    },
  };
}

describe("orchestrator /netem", () => {
  it("runs a concurrent shape and clear one after the other, never interleaved", async () => {
    const kernel = podKernel();
    const { netem, stop } = await startPod(kernel.exec);
    try {
      expect((await netem("POST", { profile: "congested_wifi" })).status).toBe(200);
      const [shape, clear] = await Promise.all([
        netem("POST", { profile: "lossy_mobile" }),
        netem("DELETE"),
      ]);
      expect([shape.status, clear.status]).toEqual([200, 200]);
      expect([
        { ethNetem: false, hook: false, redirectTarget: null, ifbNetem: false },
        { ethNetem: true, hook: true, redirectTarget: NETEM_IFB_DEV, ifbNetem: true },
      ]).toContainEqual(kernel.state());
    } finally {
      await stop();
    }
  }, 20_000);

  it("runs the next action after one that failed", async () => {
    const kernel = podKernel();
    let failNext = true;
    const { netem, stop } = await startPod(async (file, args) => {
      if (failNext) {
        failNext = false;
        throw new NetemExecError("RTNETLINK answers: Operation not permitted", 2);
      }
      return kernel.exec(file, args);
    });
    try {
      expect((await netem("POST", { profile: "congested_wifi" })).status).toBe(500);
      expect((await netem("POST", { profile: "congested_wifi" })).status).toBe(200);
    } finally {
      await stop();
    }
  }, 20_000);
});
