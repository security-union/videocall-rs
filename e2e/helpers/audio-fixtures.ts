import fs from "node:fs";
import os from "node:os";
import path from "node:path";

/**
 * Generated fake-microphone fixtures for speaking/VAD specs.
 *
 * Chromium's `--use-file-for-fake-audio-capture=<file>` replaces the fake mic
 * with the contents of a WAV file, looped for the lifetime of the browser. The
 * repo already ships a speech clip (`dioxus-ui/assets/hi.wav`) which is the
 * right fixture for "does the peer register as speaking at all" — but speech is
 * intermittent by nature, so it cannot support an assertion about the glow
 * being CONTINUOUSLY lit: the natural pauses between words legitimately drop
 * the VAD, and a spec cannot tell a legitimate pause from a regression.
 *
 * `continuousToneWavPath()` produces the complementary fixture: audio that is
 * unbroken and loud enough that the VAD verdict is constant.
 */

const SAMPLE_RATE = 48_000;
const CHANNELS = 1;
const BITS_PER_SAMPLE = 16;
const DURATION_SECONDS = 10;

/**
 * The tone's amplitude is modulated between these two values, and the choice of
 * both is load-bearing. All figures are against
 * videocall-client/src/audio_constants.rs, where the deployed `vadThreshold` is
 * 0.02 and `rms_to_intensity` saturates at `RMS_LOUD_SPEECH_CEILING` = 0.10.
 *
 * PEAK 0.6 → RMS 0.42. Four times over the saturation ceiling, so the reported
 * intensity is a clean 1.0 even if the capture path attenuates the signal.
 *
 * FLOOR 0.085 → RMS 0.060. Chosen to sit INSIDE the 0.02–0.10 band where
 * intensity actually varies (it maps to ~0.71), while still standing three
 * times clear of the VAD threshold — so the peer never stops counting as
 * speaking, and a dropped glow is never a legitimate silence.
 *
 * WHY MODULATE AT ALL: the decoder-side VAD is edge-triggered — `handle_pcm_data`
 * re-broadcasts only when the speaking boolean flips or the intensity moves by
 * more than `AUDIO_LEVEL_DELTA_THRESHOLD` (0.02) from the value it last sent. A
 * CONSTANT loud tone pins the intensity at a saturated 1.0, so after the first
 * event the fast path goes permanently silent. That is indistinguishable from a
 * dead peer, and the resolver zeroes a glow that has had no events for 12.5s —
 * which would put out the glow on CORRECT code and fail this spec for a reason
 * that has nothing to do with the bug. Sweeping the amplitude through the
 * sub-saturation band keeps real level updates flowing the whole time.
 */
const PEAK_AMPLITUDE = 0.6;
const FLOOR_AMPLITUDE = 0.085;

/**
 * Seconds per amplitude sweep. Comfortably under the 12.5s no-events deadline
 * with room to spare, and a whole divisor of `DURATION_SECONDS` so the fixture
 * loops without an amplitude step.
 */
const ENVELOPE_PERIOD_SECONDS = 2;

/**
 * The tone sweeps 300–800 Hz rather than holding one pitch. Amplitude — and
 * therefore RMS, which is all the VAD looks at — is unaffected, but a
 * continuously moving pitch is not a stationary signal, so it cannot be
 * mistaken for steady-state background noise by any noise suppression applied
 * to the capture stream.
 */
const SWEEP_LOW_HZ = 300;
const SWEEP_HIGH_HZ = 800;
const SWEEP_PERIOD_SECONDS = 2;

function encodeWav(pcm: Buffer): Buffer {
  const byteRate = (SAMPLE_RATE * CHANNELS * BITS_PER_SAMPLE) / 8;
  const blockAlign = (CHANNELS * BITS_PER_SAMPLE) / 8;

  const header = Buffer.alloc(44);
  header.write("RIFF", 0, "ascii");
  header.writeUInt32LE(36 + pcm.length, 4);
  header.write("WAVE", 8, "ascii");
  header.write("fmt ", 12, "ascii");
  header.writeUInt32LE(16, 16); // PCM fmt chunk size
  header.writeUInt16LE(1, 20); // format: PCM
  header.writeUInt16LE(CHANNELS, 22);
  header.writeUInt32LE(SAMPLE_RATE, 24);
  header.writeUInt32LE(byteRate, 28);
  header.writeUInt16LE(blockAlign, 32);
  header.writeUInt16LE(BITS_PER_SAMPLE, 34);
  header.write("data", 36, "ascii");
  header.writeUInt32LE(pcm.length, 40);

  return Buffer.concat([header, pcm]);
}

function renderSweptTone(): Buffer {
  const totalSamples = SAMPLE_RATE * DURATION_SECONDS;
  const pcm = Buffer.alloc(totalSamples * 2);
  const centreHz = (SWEEP_LOW_HZ + SWEEP_HIGH_HZ) / 2;
  const swingHz = (SWEEP_HIGH_HZ - SWEEP_LOW_HZ) / 2;

  const envelopeCentre = (PEAK_AMPLITUDE + FLOOR_AMPLITUDE) / 2;
  const envelopeSwing = (PEAK_AMPLITUDE - FLOOR_AMPLITUDE) / 2;

  // Integrate frequency into phase rather than evaluating sin(2*pi*f*t)
  // directly: with a moving `f` the latter jumps phase every sample and
  // produces broadband clicks instead of a clean sweep.
  let phase = 0;
  for (let i = 0; i < totalSamples; i += 1) {
    const t = i / SAMPLE_RATE;
    const freq = centreHz + swingHz * Math.sin((2 * Math.PI * t) / SWEEP_PERIOD_SECONDS);
    phase += (2 * Math.PI * freq) / SAMPLE_RATE;
    const amplitude =
      envelopeCentre + envelopeSwing * Math.sin((2 * Math.PI * t) / ENVELOPE_PERIOD_SECONDS);
    pcm.writeInt16LE(Math.round(Math.sin(phase) * amplitude * 32767), i * 2);
  }

  return pcm;
}

/**
 * Absolute path to a looping, constant-amplitude tone WAV suitable for
 * `--use-file-for-fake-audio-capture`.
 *
 * Written to the OS temp dir (not the repo) and regenerated only when absent,
 * so a spec run leaves no tracked artifact behind and repeat runs pay the
 * render cost once. `DURATION_SECONDS` is a whole number of sweep periods, so
 * the fixture loops without a discontinuity in pitch.
 */
export function continuousToneWavPath(): string {
  // The amplitudes are part of the cache key: they define the fixture's VAD
  // behaviour, so a run must never reuse a file rendered with different ones.
  const file = path.join(
    os.tmpdir(),
    `videocall-e2e-tone-${SAMPLE_RATE}-${DURATION_SECONDS}s-${PEAK_AMPLITUDE}-${FLOOR_AMPLITUDE}.wav`,
  );

  if (!fs.existsSync(file)) {
    fs.writeFileSync(file, encodeWav(renderSweptTone()));
  }

  return file;
}
