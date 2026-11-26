# NixOS VM test: `videocall-cli auth` device enrollment against a MOCK
# meeting-api. No database, no real server, no browser, no network.
#
# One VM, everything on loopback (the CLI allows plain http only for a loopback
# API origin): the videocall-cli binary from packages.videocall-cli plus
# mock-meeting-api.py, a short stdlib-Python server that answers the device
# endpoints the CLI calls and checks what a real server would (PKCE, the
# registration JWS signature, device JWTs, revocation). The test script plays
# the human: it opens the printed authorize URL, "approves", and pastes the
# code into the CLI's stdin (`auth login --remote`).
#
# Run (Linux with KVM only):  nix-build release.nix -A tests.cli-device-enrollment
{ p, packages }:
let
  inherit (p) pkgs;
  lib = pkgs.lib;

  apiOrigin = "http://127.0.0.1:8081";
  cli = lib.getExe packages.videocall-cli;

  # Ed25519 verification for the mock. python3's stdlib has none, and a
  # hand-rolled verifier is worse than a cached nixpkgs package.
  mockPython = pkgs.python3.withPackages (ps: [ ps.cryptography ]);

  # `auth login --remote` reads the pasted code from stdin. The fifo is opened
  # read-write (fd 3) so the CLI starts and prints the URL while the test
  # script keeps control and writes the code later. The exit status lands in
  # /tmp/login.rc so the script can tell success from failure.
  loginScript = pkgs.writeShellScript "videocall-cli-login" ''
    export HOME=/home/alice
    exec 3<>/tmp/login.in
    ${pkgs.coreutils}/bin/timeout 60 ${cli} auth login --remote --api-url ${apiOrigin} --name "nixos test device" \
      <&3 >/tmp/login.out 2>&1
    echo $? > /tmp/login.rc
  '';
in
if !pkgs.stdenv.isLinux then
  throw "tests.cli-device-enrollment is a NixOS VM test: evaluate it on a Linux system (CI), or with --argstr system x86_64-linux / aarch64-linux."
else
  pkgs.testers.runNixOSTest {
    name = "videocall-cli-device-enrollment";
    # A hang must cost minutes, not the default hour.
    globalTimeout = 300;

    nodes.machine = { pkgs, ... }: {
      systemd.services.mock-meeting-api = {
        wantedBy = [ "multi-user.target" ];
        serviceConfig.ExecStart = "${mockPython}/bin/python3 ${./mock-meeting-api.py} 8081";
      };

      users.users.alice.isNormalUser = true;
      environment.systemPackages = [ pkgs.curl ];
    };

    testScript = ''
      import json
      import re
      import shlex
      import urllib.parse

      API = "${apiOrigin}"
      CLI = "${cli}"
      LOGIN = "${loginScript}"
      KEY_DIR = "/home/alice/.config/videocall"


      def as_alice(command):
          return "su -s /bin/sh alice -c " + shlex.quote("export HOME=/home/alice; timeout 30 " + command)


      def mock(method, path):
          return json.loads(machine.succeed(f"curl -sS -X {method} {API}{path}"))


      def cli_status():
          return machine.execute(as_alice(f"{CLI} auth status --api-url {API}"))


      def start_login():
          """Start `auth login --remote`; return (authorize URL, fingerprint it printed)."""
          machine.succeed("rm -f /tmp/login.*; mkfifo -m 0666 /tmp/login.in; "
                          "install -m 0666 /dev/null /tmp/login.out")
          # The driver's execute() waits for stdout to close, so the detached
          # login must not inherit it (without this the VM test hangs).
          machine.succeed("su -s /bin/sh alice -c "
                          + shlex.quote(f"setsid -f {LOGIN} >/dev/null 2>&1 </dev/null"))
          machine.wait_until_succeeds("grep -q 'Paste the code' /tmp/login.out", timeout=30)
          out = machine.succeed("cat /tmp/login.out")
          url = re.search(r"(http://127\.0\.0\.1:8081/cli/authorize\?\S+)", out).group(1)
          fingerprint = re.search(r"Key fingerprint[^:]*: (\S+)", out).group(1)
          return url, fingerprint


      def human_approves_and_pastes(url):
          """The browser opens the URL, the human approves, the code is pasted."""
          machine.succeed("curl -sSf -o /dev/null " + shlex.quote(url))
          code = json.loads(machine.succeed(f"curl -sSf -X POST {API}/_mock/approve"))["code"]
          machine.succeed(f"echo {shlex.quote(code)} > /tmp/login.in")
          machine.wait_until_succeeds("test -s /tmp/login.rc", timeout=60)
          return int(machine.succeed("cat /tmp/login.rc")), machine.succeed("cat /tmp/login.out")


      machine.wait_for_unit("mock-meeting-api.service", timeout=60)
      machine.wait_for_open_port(8081, timeout=30)

      with subtest("not logged in yet"):
          code, out = cli_status()
          assert code == 1 and "Not logged in" in out, (code, out)

      with subtest("a rejected exchange saves nothing"):
          url, _ = start_login()
          query = {k: v[0] for k, v in urllib.parse.parse_qs(urllib.parse.urlsplit(url).query).items()}
          assert query["redirect_uri"] == "urn:ietf:wg:oauth:2.0:oob", query
          assert query["code_challenge_method"] == "S256", query
          mock("POST", "/_mock/reject-next-exchange")
          rc, out = human_approves_and_pastes(url)
          assert rc != 0 and "login failed" in out, (rc, out)
          machine.fail(f"ls {KEY_DIR}/*.toml")
          assert mock("GET", "/_mock/state")["exchanges"] == 0

      with subtest("login --remote, approved by the human"):
          url, fingerprint = start_login()
          rc, out = human_approves_and_pastes(url)
          assert rc == 0 and "Logged in as Alice (alice@example.test)" in out, (rc, out)

      with subtest("the mock saw a correct registration"):
          state = mock("GET", "/_mock/state")
          assert state["exchanges"] == 1, state
          [device] = state["devices"].values()
          assert device["name"] == "nixos test device" and not device["revoked"], device
          assert device["fingerprint"] == fingerprint, (device, fingerprint)

      with subtest("key file is private"):
          machine.succeed(f"test \"$(stat -c '%a %U' {KEY_DIR})\" = '700 alice'")
          machine.succeed(f"test \"$(ls {KEY_DIR}/*.toml | wc -l)\" = 1")
          machine.succeed(f"test \"$(stat -c '%a %U' {KEY_DIR}/*.toml)\" = '600 alice'")

      with subtest("auth status is valid"):
          code, out = cli_status()
          assert code == 0 and re.search(r"Status:\s+valid", out), (code, out)

      with subtest("revoked in the browser: auth status fails"):
          mock("POST", "/_mock/revoke-all")
          code, out = cli_status()
          assert code == 1, (code, out)

      with subtest("auth logout removes the key and revokes the device"):
          url, _ = start_login()
          rc, out = human_approves_and_pastes(url)
          assert rc == 0, (rc, out)
          code, _ = cli_status()
          assert code == 0, code
          deletes = mock("GET", "/_mock/state")["deletes"]
          machine.succeed(as_alice(f"{CLI} auth logout --api-url {API}"))
          machine.fail(f"ls {KEY_DIR}/*.toml")
          state = mock("GET", "/_mock/state")
          assert state["deletes"] == deletes + 1, state
          assert all(d["revoked"] for d in state["devices"].values()), state
    '';
  }
