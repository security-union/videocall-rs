// Pure helpers and the stream source of dioxus-ui/static/capture/track-processor.js.
// Run: node --test dioxus-ui/tests/js/track-processor.test.cjs
"use strict";
const test = require("node:test");
const assert = require("node:assert/strict");
const {
  uprightRotation,
  rotationTransform,
  createTimestampRebaser,
  videoSource,
} = require("../../static/capture/track-processor.js");

test("upright rotation matches the iPhone measurements", () => {
  // Measured on an iPhone 18 Pro (iOS 27.2.1, Safari 27.2): degrees
  // clockwise that turn a worker MediaStreamTrackProcessor frame upright.
  const measured = [
    // [screen.orientation.angle, facingMode, rotation]
    [0, "user", 90],
    [0, "environment", 90],
    [90, "user", 180],
    [90, "environment", 0],
    [270, "user", 0],
    [270, "environment", 180],
  ];
  for (const [angle, facing, rotation] of measured) {
    assert.equal(uprightRotation(angle, facing), rotation, `angle ${angle}, ${facing}`);
  }
});

test("upright rotation treats a missing facingMode as the back camera", () => {
  assert.equal(uprightRotation(90, undefined), 0);
});

test("upright rotation accepts window.orientation values", () => {
  // window.orientation reports -90 for what screen.orientation calls 270.
  assert.equal(uprightRotation(-90, "user"), uprightRotation(270, "user"));
  assert.equal(uprightRotation(-90, "environment"), uprightRotation(270, "environment"));
});

test("rotation transform lands the frame inside the output canvas", () => {
  const apply = ([a, b, c, d, e, f], [x, y]) => [a * x + c * y + e, b * x + d * y + f];
  const width = 1280;
  const height = 720;
  // Where the source's top-left corner ends up after a clockwise turn.
  const expected = {
    0: { size: [1280, 720], topLeft: [0, 0], bottomRight: [1280, 720] },
    90: { size: [720, 1280], topLeft: [720, 0], bottomRight: [0, 1280] },
    180: { size: [1280, 720], topLeft: [1280, 720], bottomRight: [0, 0] },
    270: { size: [720, 1280], topLeft: [0, 1280], bottomRight: [720, 0] },
  };
  for (const [rotation, want] of Object.entries(expected)) {
    const transform = rotationTransform(Number(rotation), width, height);
    assert.deepEqual([transform.width, transform.height], want.size, `${rotation}: size`);
    assert.deepEqual(apply(transform.matrix, [0, 0]), want.topLeft, `${rotation}: top-left`);
    assert.deepEqual(
      apply(transform.matrix, [width, height]),
      want.bottomRight,
      `${rotation}: bottom-right`,
    );
  }
});

test("rebased timestamps start at the main-thread clock and keep their spacing", () => {
  let nowUs = 5_000_000;
  const rebase = createTimestampRebaser(() => nowUs);
  assert.equal(rebase(100), 5_000_000);
  nowUs = 9_999_999; // later reads of the clock do not move the anchor
  assert.equal(rebase(33_433), 5_033_333);
  assert.equal(rebase(66_766), 5_066_666);
});

test("rebased timestamps stay strictly increasing when the source goes backwards", () => {
  const rebase = createTimestampRebaser(() => 1_000_000);
  const first = rebase(500_000);
  const second = rebase(533_333);
  const backwards = rebase(0);
  const after = rebase(33_333);
  assert.ok(backwards > second, "a backwards jump still moves forward");
  assert.equal(after - backwards, 33_333, "spacing is kept after re-anchoring");
  assert.equal(second - first, 33_333);
});


// A capture ({pull, cancel}) that records whether it was cancelled.
function fakeCapture() {
  return {
    cancelled: false,
    pull: () => Promise.resolve(null),
    cancel() {
      this.cancelled = true;
    },
  };
}

test("a stream cancelled while its capture is starting cancels that capture", async () => {
  let finishStart;
  let sawCancelled = null;
  const startPromise = new Promise((resolve) => (finishStart = resolve));
  const stream = new ReadableStream(
    videoSource((isCancelled) =>
      startPromise.then((capture) => {
        sawCancelled = isCancelled();
        return capture;
      }),
    ),
    { highWaterMark: 0 },
  );
  // ReadableStream runs the source's cancel() without waiting for start().
  const cancelled = stream.getReader().cancel();
  const capture = fakeCapture();
  finishStart(capture);
  await cancelled;
  await new Promise((resolve) => setImmediate(resolve));
  assert.equal(sawCancelled, true, "start() can see the cancellation");
  assert.equal(capture.cancelled, true, "the late capture is cancelled");
});

test("a stream cancelled after start cancels its capture once", async () => {
  let cancels = 0;
  const capture = { ...fakeCapture(), cancel: () => cancels++ };
  const stream = new ReadableStream(
    videoSource(() => Promise.resolve(capture)),
    { highWaterMark: 0 },
  );
  await new Promise((resolve) => setImmediate(resolve));
  await stream.getReader().cancel();
  assert.equal(cancels, 1);
});
