import { mkdtempSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { afterEach, beforeEach, describe, expect, it } from "vitest";

import type { BotTask } from "../orchestrator";
import { generateToken } from "./auth";
import {
  type ControlServerHandle,
  type OrchestratorControlSurface,
  startControlServer,
} from "./server";
import { type BotRegistryEntry, generateBotId, newRegistryEntry } from "./registry";

function fakeTask(overrides: Partial<BotTask> = {}): BotTask {
  return {
    botId: generateBotId(),
    meetingURL: "https://example.com/meeting/X",
    participant: "alice",
    displayName: "Alice",
    headless: false,
    authBackend: "jwt",
    storageStateFile: null,
    ssoStateFile: null,
    manifest: null,
    runDir: null,
    ttl: 300_000,
    network: null,
    ...overrides,
  };
}

interface MockSurface extends OrchestratorControlSurface {
  registry: Map<string, BotRegistryEntry>;
  callLog: string[];
}

function mockSurface(initial: BotRegistryEntry[] = []): MockSurface {
  const registry = new Map<string, BotRegistryEntry>();
  for (const e of initial) registry.set(e.botId, e);
  const callLog: string[] = [];
  const surface: MockSurface = {
    registry,
    callLog,
    getRegistry: () => registry,
    triggerLeave: async (id) => void callLog.push(`leave:${id}`),
    forceKill: async (id) => void callLog.push(`kill:${id}`),
    applyTtl: (id, ttl) => void callLog.push(`ttl:${id}:${ttl}`),
    changeNetwork: async (id, n) => void callLog.push(`network:${id}:${n}`),
    setMicMuted: async (id, m) => void callLog.push(`mic:${id}:${m}`),
    setCameraOff: async (id, c) => void callLog.push(`cam:${id}:${c}`),
    setScreenShare: async (id, s) => void callLog.push(`share:${id}:${s}`),
    duplicateBot: async (id, ov) => {
      const newId = generateBotId();
      callLog.push(`dup:${id}->${newId}:${JSON.stringify(ov)}`);
      return newId;
    },
    launchOne: async (spec) => {
      const newId = generateBotId();
      callLog.push(`launch:${newId}:${JSON.stringify(spec)}`);
      return newId;
    },
  };
  return surface;
}

async function fetchJson(
  port: number,
  path: string,
  init: { method?: string; headers?: Record<string, string>; body?: unknown } = {},
): Promise<{ status: number; body: unknown }> {
  const headers: Record<string, string> = { accept: "application/json", ...init.headers };
  let body: string | undefined;
  if (init.body !== undefined) {
    body = JSON.stringify(init.body);
    headers["content-type"] = "application/json";
  }
  const res = await fetch(`http://127.0.0.1:${port}${path}`, {
    method: init.method ?? "GET",
    headers,
    body,
  });
  const text = await res.text();
  return {
    status: res.status,
    body: text.length === 0 ? null : JSON.parse(text),
  };
}

describe("control server: SSH host registry endpoints", () => {
  let handle: ControlServerHandle;
  let token: string;
  let surface: MockSurface;
  let runDir: string;

  beforeEach(async () => {
    token = generateToken();
    surface = mockSurface();
    runDir = mkdtempSync(join(tmpdir(), "bots-server-hosts-"));
    handle = await startControlServer({ port: 0, token, surface, runDir });
  });

  afterEach(async () => {
    await handle.close();
  });

  it("GET /hosts returns an empty list on a fresh runDir", async () => {
    const res = await fetchJson(handle.port, "/hosts", {
      headers: { authorization: `Bearer ${token}` },
    });
    expect(res.status).toBe(200);
    expect(res.body).toEqual({ hosts: [] });
  });

  it("POST /hosts creates a host and returns 201", async () => {
    const res = await fetchJson(handle.port, "/hosts", {
      method: "POST",
      headers: { authorization: `Bearer ${token}` },
      body: {
        label: "mini-7",
        host: "mini-7.intra",
        user: "alice",
        reposPath: "/home/alice/videocall",
      },
    });
    expect(res.status).toBe(201);
    const body = res.body as { host: { label: string } };
    expect(body.host.label).toBe("mini-7");
  });

  it("POST /hosts rejects a duplicate label with 409", async () => {
    await fetchJson(handle.port, "/hosts", {
      method: "POST",
      headers: { authorization: `Bearer ${token}` },
      body: {
        label: "dup",
        host: "h",
        user: "alice",
        reposPath: "/home/alice/videocall",
      },
    });
    const res = await fetchJson(handle.port, "/hosts", {
      method: "POST",
      headers: { authorization: `Bearer ${token}` },
      body: {
        label: "dup",
        host: "h2",
        user: "bob",
        reposPath: "/home/bob/videocall",
      },
    });
    expect(res.status).toBe(409);
  });

  it("POST /hosts rejects invalid label with 400", async () => {
    const res = await fetchJson(handle.port, "/hosts", {
      method: "POST",
      headers: { authorization: `Bearer ${token}` },
      body: {
        label: "-bad",
        host: "h",
        user: "alice",
        reposPath: "/home/alice/videocall",
      },
    });
    expect(res.status).toBe(400);
  });

  it("PUT /hosts/:label patches a host", async () => {
    await fetchJson(handle.port, "/hosts", {
      method: "POST",
      headers: { authorization: `Bearer ${token}` },
      body: {
        label: "patch",
        host: "old",
        user: "alice",
        reposPath: "/home/alice/videocall",
      },
    });
    const res = await fetchJson(handle.port, "/hosts/patch", {
      method: "PUT",
      headers: { authorization: `Bearer ${token}` },
      body: { host: "new" },
    });
    expect(res.status).toBe(200);
    const body = res.body as { host: { host: string } };
    expect(body.host.host).toBe("new");
  });

  it("POST /hosts persists the structured shell/profileFile/preCommand fields and surfaces them on GET", async () => {
    const post = await fetchJson(handle.port, "/hosts", {
      method: "POST",
      headers: { authorization: `Bearer ${token}` },
      body: {
        label: "zsh-mac",
        host: "h",
        user: "alice",
        reposPath: "/home/alice/videocall",
        shell: "zsh",
        profileFile: "~/.zshrc",
        preCommand: ". ~/.nvm/nvm.sh && nvm use 22",
      },
    });
    expect(post.status).toBe(201);
    const created = (
      post.body as {
        host: { shell: string | null; profileFile: string | null; preCommand: string | null };
      }
    ).host;
    expect(created.shell).toBe("zsh");
    expect(created.profileFile).toBe("~/.zshrc");
    expect(created.preCommand).toBe(". ~/.nvm/nvm.sh && nvm use 22");
    // Round-trip via GET to confirm persistence.
    const list = await fetchJson(handle.port, "/hosts", {
      headers: { authorization: `Bearer ${token}` },
    });
    const found = (
      list.body as {
        hosts: Array<{
          label: string;
          shell: string | null;
          profileFile: string | null;
          preCommand: string | null;
        }>;
      }
    ).hosts.find((h) => h.label === "zsh-mac");
    expect(found?.shell).toBe("zsh");
    expect(found?.profileFile).toBe("~/.zshrc");
    expect(found?.preCommand).toBe(". ~/.nvm/nvm.sh && nvm use 22");
  });

  it("POST /hosts defaults shell/profileFile/preCommand to null when omitted", async () => {
    const post = await fetchJson(handle.port, "/hosts", {
      method: "POST",
      headers: { authorization: `Bearer ${token}` },
      body: {
        label: "default-init",
        host: "h",
        user: "alice",
        reposPath: "/home/alice/videocall",
      },
    });
    expect(post.status).toBe(201);
    const created = (
      post.body as {
        host: { shell: string | null; profileFile: string | null; preCommand: string | null };
      }
    ).host;
    expect(created.shell).toBeNull();
    expect(created.profileFile).toBeNull();
    expect(created.preCommand).toBeNull();
  });

  it("PUT /hosts/:label updates the structured fields", async () => {
    await fetchJson(handle.port, "/hosts", {
      method: "POST",
      headers: { authorization: `Bearer ${token}` },
      body: {
        label: "patch-init",
        host: "h",
        user: "alice",
        reposPath: "/home/alice/videocall",
      },
    });
    const put = await fetchJson(handle.port, "/hosts/patch-init", {
      method: "PUT",
      headers: { authorization: `Bearer ${token}` },
      body: { shell: "zsh", profileFile: "~/.zshrc", preCommand: ". ~/.nvm/nvm.sh" },
    });
    expect(put.status).toBe(200);
    const patched = (
      put.body as {
        host: { shell: string | null; profileFile: string | null; preCommand: string | null };
      }
    ).host;
    expect(patched.shell).toBe("zsh");
    expect(patched.profileFile).toBe("~/.zshrc");
    expect(patched.preCommand).toBe(". ~/.nvm/nvm.sh");
    // Clearing (`null`) round-trips.
    const cleared = await fetchJson(handle.port, "/hosts/patch-init", {
      method: "PUT",
      headers: { authorization: `Bearer ${token}` },
      body: { shell: null, profileFile: null, preCommand: null },
    });
    expect(cleared.status).toBe(200);
    const clearedHost = (
      cleared.body as {
        host: { shell: string | null; profileFile: string | null; preCommand: string | null };
      }
    ).host;
    expect(clearedHost.shell).toBeNull();
    expect(clearedHost.profileFile).toBeNull();
    expect(clearedHost.preCommand).toBeNull();
  });

  it("POST /hosts rejects preCommand longer than 512 chars with 400", async () => {
    const res = await fetchJson(handle.port, "/hosts", {
      method: "POST",
      headers: { authorization: `Bearer ${token}` },
      body: {
        label: "too-long",
        host: "h",
        user: "alice",
        reposPath: "/home/alice/videocall",
        preCommand: "a".repeat(513),
      },
    });
    expect(res.status).toBe(400);
  });

  it("POST /hosts rejects preCommand with embedded newlines as 400", async () => {
    const res = await fetchJson(handle.port, "/hosts", {
      method: "POST",
      headers: { authorization: `Bearer ${token}` },
      body: {
        label: "with-newline",
        host: "h",
        user: "alice",
        reposPath: "/home/alice/videocall",
        preCommand: ". ~/.zshrc\nrm -rf /",
      },
    });
    expect(res.status).toBe(400);
  });

  it("POST /hosts rejects a shell value containing shell metacharacters with 400", async () => {
    const res = await fetchJson(handle.port, "/hosts", {
      method: "POST",
      headers: { authorization: `Bearer ${token}` },
      body: {
        label: "shell-bad",
        host: "h",
        user: "alice",
        reposPath: "/home/alice/videocall",
        shell: "bash;rm -rf /",
      },
    });
    expect(res.status).toBe(400);
  });

  it("DELETE /hosts/:label removes the row", async () => {
    await fetchJson(handle.port, "/hosts", {
      method: "POST",
      headers: { authorization: `Bearer ${token}` },
      body: {
        label: "gone",
        host: "h",
        user: "alice",
        reposPath: "/home/alice/videocall",
      },
    });
    const res = await fetchJson(handle.port, "/hosts/gone", {
      method: "DELETE",
      headers: { authorization: `Bearer ${token}` },
    });
    expect(res.status).toBe(204);
    const list = await fetchJson(handle.port, "/hosts", {
      headers: { authorization: `Bearer ${token}` },
    });
    expect(list.body).toEqual({ hosts: [] });
  });

  it("DELETE on a missing host returns 404", async () => {
    const res = await fetchJson(handle.port, "/hosts/ghost", {
      method: "DELETE",
      headers: { authorization: `Bearer ${token}` },
    });
    expect(res.status).toBe(404);
  });

  describe("POST /hosts/:label/preview-launch", () => {
    async function seed(label: string): Promise<void> {
      await fetchJson(handle.port, "/hosts", {
        method: "POST",
        headers: { authorization: `Bearer ${token}` },
        body: {
          label,
          host: "my-host.lan:2222",
          user: "alice",
          sshKey: null,
          reposPath: "/home/alice/videocall",
        },
      });
    }

    it("returns argv + display + remoteCommand for a valid spec", async () => {
      await seed("preview-ok");
      const res = await fetchJson(handle.port, "/hosts/preview-ok/preview-launch", {
        method: "POST",
        headers: { authorization: `Bearer ${token}` },
        body: {
          meetingURL: "https://example.com/meeting/X",
          participant: "alice",
          ttl: "5m",
          headless: true,
          network: "none",
          authBackend: "jwt",
        },
      });
      expect(res.status).toBe(200);
      const body = res.body as { argv: string[]; display: string; remoteCommand: string };
      expect(Array.isArray(body.argv)).toBe(true);
      expect(body.argv[0]).toBe("ssh");
      expect(body.argv).toContain("alice@my-host.lan");
      expect(body.argv).toContain("-p");
      expect(body.argv).toContain("2222");
      expect(body.display).toMatch(/^ssh /);
      expect(body.display).toContain("ConnectTimeout=10");
      expect(body.remoteCommand).toContain("npm run bot");
      expect(body.remoteCommand).toContain("--participant 'alice'");
      expect(body.remoteCommand).toContain("--ttl '5m'");
    });

    it("returns 404 when the host is not registered", async () => {
      const res = await fetchJson(handle.port, "/hosts/ghost/preview-launch", {
        method: "POST",
        headers: { authorization: `Bearer ${token}` },
        body: {
          meetingURL: "https://example.com/meeting/X",
          participant: "alice",
          ttl: "5m",
          headless: true,
          network: "none",
          authBackend: "jwt",
        },
      });
      expect(res.status).toBe(404);
    });

    it("returns 400 for an invalid participant", async () => {
      await seed("preview-bad");
      const res = await fetchJson(handle.port, "/hosts/preview-bad/preview-launch", {
        method: "POST",
        headers: { authorization: `Bearer ${token}` },
        body: {
          meetingURL: "https://example.com/meeting/X",
          participant: "alice space",
          ttl: "5m",
          headless: true,
          network: "none",
          authBackend: "jwt",
        },
      });
      expect(res.status).toBe(400);
    });

    it("returns 400 for an unknown network preset", async () => {
      await seed("preview-bad-net");
      const res = await fetchJson(handle.port, "/hosts/preview-bad-net/preview-launch", {
        method: "POST",
        headers: { authorization: `Bearer ${token}` },
        body: {
          meetingURL: "https://example.com/meeting/X",
          participant: "alice",
          ttl: "5m",
          headless: true,
          network: "no-such-profile",
          authBackend: "jwt",
        },
      });
      expect(res.status).toBe(400);
    });

    it("rejects unauthenticated preview requests with 401", async () => {
      await seed("preview-auth");
      const res = await fetchJson(handle.port, "/hosts/preview-auth/preview-launch", {
        method: "POST",
      });
      expect(res.status).toBe(401);
    });
  });

  describe("POST /hosts/preview (unsaved host)", () => {
    it("returns argv + display + remoteCommand for a valid unsaved host", async () => {
      const res = await fetchJson(handle.port, "/hosts/preview", {
        method: "POST",
        headers: { authorization: `Bearer ${token}` },
        body: {
          host: {
            label: "ephemeral",
            host: "lab.intra",
            user: "alice",
            reposPath: "/home/alice/videocall",
            shell: "bash",
            profileFile: "~/.bash_profile",
            preCommand: ". ~/.nvm/nvm.sh && nvm use 22",
          },
        },
      });
      expect(res.status).toBe(200);
      const body = res.body as { argv: string[]; display: string; remoteCommand: string };
      expect(Array.isArray(body.argv)).toBe(true);
      expect(body.argv[0]).toBe("ssh");
      expect(body.argv).toContain("alice@lab.intra");
      expect(body.display).toMatch(/^ssh /);
      // Default placeholder tokens are visible (no real participant /
      // meeting URL supplied).
      expect(body.remoteCommand).toContain("--participant '<participant>'");
      expect(body.remoteCommand).toContain("--meeting-url '<meeting-url>'");
      // The structured prefix shows up in the wrapper payload (last argv slot).
      const tail = body.argv[body.argv.length - 1];
      expect(tail).toContain("[ -f ~/.bash_profile ] && . ~/.bash_profile;");
      expect(tail).toContain(". ~/.nvm/nvm.sh && nvm use 22;");
    });

    it("uses the host.shell value as the wrapper shell (zsh)", async () => {
      const res = await fetchJson(handle.port, "/hosts/preview", {
        method: "POST",
        headers: { authorization: `Bearer ${token}` },
        body: {
          host: {
            label: "zsh-host",
            host: "lab.intra",
            user: "alice",
            reposPath: "/home/alice/videocall",
            shell: "zsh",
            profileFile: "~/.zshrc",
          },
        },
      });
      expect(res.status).toBe(200);
      const body = res.body as { argv: string[]; display: string };
      const tail = body.argv[body.argv.length - 1];
      expect(tail.startsWith("zsh -lc ")).toBe(true);
      expect(tail).toContain("[ -f ~/.zshrc ] && . ~/.zshrc;");
    });

    it("returns 400 when the inner host is missing required fields", async () => {
      const res = await fetchJson(handle.port, "/hosts/preview", {
        method: "POST",
        headers: { authorization: `Bearer ${token}` },
        body: {
          host: {
            // Missing `reposPath`.
            label: "broken",
            host: "h",
            user: "alice",
          },
        },
      });
      expect(res.status).toBe(400);
    });

    it("returns 400 when the host body is not an object", async () => {
      const res = await fetchJson(handle.port, "/hosts/preview", {
        method: "POST",
        headers: { authorization: `Bearer ${token}` },
        body: { host: "not-an-object" },
      });
      expect(res.status).toBe(400);
    });

    it("returns 400 when the shell value contains metacharacters", async () => {
      const res = await fetchJson(handle.port, "/hosts/preview", {
        method: "POST",
        headers: { authorization: `Bearer ${token}` },
        body: {
          host: {
            label: "evil",
            host: "h",
            user: "alice",
            reposPath: "/home/alice/videocall",
            shell: "bash;rm -rf /",
          },
        },
      });
      expect(res.status).toBe(400);
    });

    it("rejects unauthenticated /hosts/preview requests with 401", async () => {
      const res = await fetchJson(handle.port, "/hosts/preview", {
        method: "POST",
        body: {
          host: {
            label: "anon",
            host: "h",
            user: "alice",
            reposPath: "/home/alice/videocall",
          },
        },
      });
      expect(res.status).toBe(401);
    });

    it("does NOT persist the previewed host (preview is read-only)", async () => {
      await fetchJson(handle.port, "/hosts/preview", {
        method: "POST",
        headers: { authorization: `Bearer ${token}` },
        body: {
          host: {
            label: "ghost",
            host: "h",
            user: "alice",
            reposPath: "/home/alice/videocall",
          },
        },
      });
      // The registry must still be empty after the preview call.
      const list = await fetchJson(handle.port, "/hosts", {
        headers: { authorization: `Bearer ${token}` },
      });
      expect((list.body as { hosts: unknown[] }).hosts).toEqual([]);
    });
  });

  it("GET /bots/:id/log returns empty list for a local bot", async () => {
    const entry = newRegistryEntry(fakeTask());
    surface.registry.set(entry.botId, entry);
    const res = await fetchJson(handle.port, `/bots/${entry.botId}/log`, {
      headers: { authorization: `Bearer ${token}` },
    });
    expect(res.status).toBe(200);
    expect(res.body).toEqual({ lines: [], totalLines: 0 });
  });

  it("GET /bots/:id/log returns 404 for an unknown bot id", async () => {
    const res = await fetchJson(handle.port, "/bots/nope/log", {
      headers: { authorization: `Bearer ${token}` },
    });
    expect(res.status).toBe(404);
  });

  it("rejects unauthenticated requests against /hosts with 401", async () => {
    const res = await fetchJson(handle.port, "/hosts");
    expect(res.status).toBe(401);
  });

  it("returns 503 when runDir is not configured", async () => {
    const noRunDirHandle = await startControlServer({
      port: 0,
      token,
      surface: mockSurface(),
    });
    try {
      const res = await fetchJson(noRunDirHandle.port, "/hosts", {
        headers: { authorization: `Bearer ${token}` },
      });
      expect(res.status).toBe(503);
    } finally {
      await noRunDirHandle.close();
    }
  });

  it("listHosts surfaces malformed JSON on disk as 400", async () => {
    writeFileSync(join(runDir, "hosts.json"), "{ bad", "utf8");
    const res = await fetchJson(handle.port, "/hosts", {
      headers: { authorization: `Bearer ${token}` },
    });
    expect(res.status).toBe(400);
  });
});

describe("control server: launch with runLocation", () => {
  let handle: ControlServerHandle;
  let token: string;
  let surface: MockSurface;
  let runDir: string;

  beforeEach(async () => {
    token = generateToken();
    surface = mockSurface();
    runDir = mkdtempSync(join(tmpdir(), "bots-server-runloc-"));
    handle = await startControlServer({ port: 0, token, surface, runDir });
  });

  afterEach(async () => {
    await handle.close();
  });

  it('accepts runLocation = { kind: "local" } and forwards to surface.launchOne', async () => {
    const res = await fetchJson(handle.port, "/launch", {
      method: "POST",
      headers: { authorization: `Bearer ${token}` },
      body: {
        meetingURL: "https://example.com/meeting/X",
        participant: "alice",
        ttl: "5m",
        headless: true,
        network: "none",
        authBackend: "jwt",
        runLocation: { kind: "local" },
      },
    });
    expect(res.status).toBe(201);
    expect(surface.callLog.some((l) => l.startsWith("launch:"))).toBe(true);
  });

  it('accepts runLocation = { kind: "ssh", hostLabel }', async () => {
    const res = await fetchJson(handle.port, "/launch", {
      method: "POST",
      headers: { authorization: `Bearer ${token}` },
      body: {
        meetingURL: "https://example.com/meeting/X",
        participant: "alice",
        ttl: "5m",
        headless: true,
        network: "none",
        authBackend: "jwt",
        runLocation: { kind: "ssh", hostLabel: "mini-7" },
      },
    });
    expect(res.status).toBe(201);
    const launchEntry = surface.callLog.find((l) => l.startsWith("launch:"));
    expect(launchEntry).toBeDefined();
    expect(launchEntry!).toContain('"hostLabel":"mini-7"');
  });

  it("rejects runLocation.kind=ssh without hostLabel as 400", async () => {
    const res = await fetchJson(handle.port, "/launch", {
      method: "POST",
      headers: { authorization: `Bearer ${token}` },
      body: {
        meetingURL: "https://example.com/meeting/X",
        participant: "alice",
        ttl: "5m",
        headless: true,
        network: "none",
        authBackend: "jwt",
        runLocation: { kind: "ssh" },
      },
    });
    expect(res.status).toBe(400);
  });

  it("rejects bare-string future-* runLocation values as 400", async () => {
    const res = await fetchJson(handle.port, "/launch", {
      method: "POST",
      headers: { authorization: `Bearer ${token}` },
      body: {
        meetingURL: "https://example.com/meeting/X",
        participant: "alice",
        ttl: "5m",
        headless: true,
        network: "none",
        authBackend: "jwt",
        runLocation: "future-vm",
      },
    });
    expect(res.status).toBe(400);
  });

  it("accepts bare-string 'local' for back-compat with pre-SSH clients", async () => {
    const res = await fetchJson(handle.port, "/launch", {
      method: "POST",
      headers: { authorization: `Bearer ${token}` },
      body: {
        meetingURL: "https://example.com/meeting/X",
        participant: "alice",
        ttl: "5m",
        headless: true,
        network: "none",
        authBackend: "jwt",
        runLocation: "local",
      },
    });
    expect(res.status).toBe(201);
  });
});
