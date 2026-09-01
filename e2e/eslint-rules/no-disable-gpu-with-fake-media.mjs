const DISABLE_GPU = "--disable-gpu";
const FAKE_DEVICE = "--use-fake-device-for-media-stream";

function staticString(node) {
  if (node === undefined || node === null) return null;
  if (node.type === "Literal" && typeof node.value === "string") return node.value;
  if (node.type === "TemplateLiteral" && node.expressions.length === 0) {
    return node.quasis.map((q) => q.value.cooked ?? "").join("");
  }
  return null;
}

export default {
  meta: {
    type: "problem",
    docs: {
      description:
        "A Chrome argument list that opens a camera must not carry --disable-gpu. Under that flag getUserMedia still resolves with a `live` track, but the track delivers zero frames to any sink, so every camera-dependent spec fails with nothing to point at (issue #2193). Keyed on the presence of --use-fake-device-for-media-stream in the same array, so a browser that never requests a camera — e.g. the wasm warmup in global-setup.ts — is exempt without an allowlist. Matches the fake-device flag bare or valued (--use-fake-device-for-media-stream=...). Catches the static array-literal form only: args assembled by spread, concat or a variable are not seen.",
    },
    schema: [],
    messages: {
      disableGpuWithFakeMedia:
        "`--disable-gpu` in an argument list that also passes `--use-fake-device-for-media-stream`. The flag starves Chrome's fake camera: the track reports readyState 'live' at 640x480 and delivers zero frames, so `video.play()` never settles and no VIDEO packet is ever published (issue #2193). Remove it. A browser that does not open a camera may keep the flag.",
    },
  },

  create(context) {
    return {
      ArrayExpression(node) {
        const values = node.elements.map((el) => staticString(el));
        if (!values.some((v) => v === FAKE_DEVICE || v?.startsWith(`${FAKE_DEVICE}=`))) return;
        node.elements.forEach((el, i) => {
          if (values[i] === DISABLE_GPU) {
            context.report({ node: el, messageId: "disableGpuWithFakeMedia" });
          }
        });
      },
    };
  },
};
