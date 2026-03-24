#!/usr/bin/env python3
"""Generate interleaved conversation WAV files and EKG video frames using Piper TTS.

Produces per bot:
  - conversation-alice.wav / conversation-bob.wav  (48kHz mono)
  - frames-alice/frame_NNNNN.jpg                   (1280x720, 15fps)
  - frames-bob/frame_NNNNN.jpg

Each bot plays its WAV + frame sequence on loop. Since both are the same
length with complementary speech/silence, the bots take turns naturally.
The video frames show an EKG-style waveform that tracks the audio amplitude.
"""

import numpy as np
import wave
from math import gcd
from pathlib import Path

from PIL import Image, ImageDraw, ImageFont
from scipy.signal import resample_poly

from piper import PiperVoice

VOICES_DIR = Path(__file__).parent / "voices"
OUTPUT_DIR = Path(__file__).parent / "conversation"
TARGET_SR = 48000   # Opus encoder expects 48kHz
PAUSE_MS = 800      # silence between lines
VIDEO_FPS = 15
VIDEO_W = 1280
VIDEO_H = 720

ALICE_COLOR = (0, 200, 220)   # cyan
BOB_COLOR = (80, 220, 80)     # green
BG_COLOR = (20, 20, 30)       # dark blue-gray
GRID_COLOR = (40, 40, 55)     # subtle grid
FLAT_COLOR = (60, 60, 80)     # dim flat line when silent
TEXT_COLOR = (180, 180, 200)

CONVERSATION = [
    ("alice", "Hello there! My name is Alice. Nice to meet you."),
    ("bob",   "Hey Alice! I'm Bob. Great to meet you too."),
    ("alice", "So Bob, what's the most interesting thing you've learned recently?"),
    ("bob",   "I just learned that honey never spoils. Archaeologists found 3000 year old honey in Egyptian tombs and it was still edible."),
    ("alice", "That's amazing! I read that octopuses have three hearts and blue blood."),
    ("bob",   "Nature is incredible. Did you know that a group of flamingos is called a flamboyance?"),
    ("alice", "Ha! That's perfect. Here's one more: bananas are technically berries, but strawberries aren't."),
    ("bob",   "Mind blown! This has been a great chat, Alice."),
    ("alice", "Agreed! Let's do this again sometime, Bob."),
    ("bob",   "Absolutely. Talk to you soon!"),
]


def synthesize_line(voice: PiperVoice, text: str) -> np.ndarray:
    chunks = list(voice.synthesize(text))
    return np.concatenate([c.audio_float_array for c in chunks])


def resample(audio: np.ndarray, from_sr: int, to_sr: int) -> np.ndarray:
    if from_sr == to_sr:
        return audio
    g = gcd(from_sr, to_sr)
    return resample_poly(audio, to_sr // g, from_sr // g).astype(np.float32)


def silence(duration_ms: int, sample_rate: int) -> np.ndarray:
    return np.zeros(int(sample_rate * duration_ms / 1000), dtype=np.float32)


def write_wav(path: Path, audio: np.ndarray, sample_rate: int):
    int16_audio = np.clip(audio * 32767, -32768, 32767).astype(np.int16)
    with wave.open(str(path), "wb") as wf:
        wf.setnchannels(1)
        wf.setsampwidth(2)
        wf.setframerate(sample_rate)
        wf.writeframes(int16_audio.tobytes())


# ---------------------------------------------------------------------------
# Video frame generation
# ---------------------------------------------------------------------------

def compute_rms_per_frame(audio: np.ndarray, sample_rate: int, fps: int) -> np.ndarray:
    """Compute smoothed RMS amplitude for each video frame's worth of audio.

    Uses a ~300ms sliding window to soften transitions between speaking and
    silence.  This makes small A/V timing offsets much less perceptible —
    the waveform fades in/out rather than snapping instantly.
    """
    samples_per_frame = sample_rate // fps
    n_frames = len(audio) // samples_per_frame
    rms = np.zeros(n_frames, dtype=np.float32)
    for i in range(n_frames):
        chunk = audio[i * samples_per_frame : (i + 1) * samples_per_frame]
        rms[i] = np.sqrt(np.mean(chunk ** 2))

    # Smooth with a ~300ms window (fade in/out instead of hard cut)
    smooth_frames = max(1, int(0.3 * fps))  # ~4-5 frames at 15fps
    kernel = np.ones(smooth_frames) / smooth_frames
    rms = np.convolve(rms, kernel, mode='same').astype(np.float32)
    return rms


def try_load_font(size: int):
    """Try to load a monospace font, fall back to default."""
    for name in [
        "/usr/share/fonts/truetype/dejavu/DejaVuSansMono-Bold.ttf",
        "/usr/share/fonts/truetype/liberation/LiberationMono-Bold.ttf",
        "/usr/share/fonts/TTF/DejaVuSansMono-Bold.ttf",
    ]:
        try:
            return ImageFont.truetype(name, size)
        except (OSError, IOError):
            continue
    return ImageFont.load_default()


def render_frame(
    name: str,
    color: tuple,
    rms_value: float,
    max_rms: float,
    frame_idx: int,
    font_large,
    font_small,
) -> Image.Image:
    """Render a single 1280x720 EKG-style frame."""
    img = Image.new("RGB", (VIDEO_W, VIDEO_H), BG_COLOR)
    draw = ImageDraw.Draw(img)

    # Draw subtle horizontal grid lines
    center_y = VIDEO_H // 2
    for dy in range(-300, 301, 60):
        y = center_y + dy
        draw.line([(0, y), (VIDEO_W, y)], fill=GRID_COLOR, width=1)

    # Draw center baseline
    draw.line([(0, center_y), (VIDEO_W, center_y)], fill=FLAT_COLOR, width=2)

    # Determine if speaking (RMS above noise floor)
    is_speaking = rms_value > 0.01 and max_rms > 0.01

    if is_speaking:
        # Normalize amplitude (0-1)
        amplitude = min(rms_value / max_rms, 1.0)
        wave_height = int(amplitude * 280)  # max ±280 pixels from center

        # Draw EKG-style waveform across the frame width
        # Use a combination of sine waves modulated by amplitude for organic look
        points = []
        n_points = VIDEO_W
        phase = frame_idx * 0.3  # animate across frames
        for x in range(n_points):
            t = x / n_points * 12 * np.pi + phase
            # Multi-frequency waveform for EKG character
            val = (
                np.sin(t) * 0.5
                + np.sin(t * 2.3) * 0.3
                + np.sin(t * 5.7) * 0.15
                + np.sin(t * 0.7) * 0.05
            )
            y = center_y - int(val * wave_height)
            points.append((x, y))

        # Draw the waveform with glow effect
        # Outer glow (wider, dimmer)
        glow_color = tuple(c // 3 for c in color)
        draw.line(points, fill=glow_color, width=5)
        # Main line
        draw.line(points, fill=color, width=2)

        # Draw amplitude bar on the right side
        bar_x = VIDEO_W - 40
        bar_top = center_y - wave_height
        bar_bot = center_y + wave_height
        draw.rectangle(
            [(bar_x, bar_top), (bar_x + 20, bar_bot)],
            fill=color,
            outline=None,
        )
    else:
        # Flat line with subtle pulse
        pulse_x = (frame_idx * 8) % VIDEO_W
        points = []
        for x in range(VIDEO_W):
            dist = abs(x - pulse_x)
            if dist < 40:
                bump = int(8 * np.exp(-(dist ** 2) / 200))
            else:
                bump = 0
            points.append((x, center_y - bump))
        draw.line(points, fill=FLAT_COLOR, width=2)

    # Draw name label
    draw.text((30, 30), name, fill=color, font=font_large)

    # Draw status
    status = "SPEAKING" if is_speaking else "LISTENING"
    draw.text((30, 80), status, fill=TEXT_COLOR, font=font_small)

    return img


def generate_frames(
    name: str,
    color: tuple,
    audio: np.ndarray,
    output_dir: Path,
):
    """Generate all video frames for one bot."""
    output_dir.mkdir(parents=True, exist_ok=True)

    rms = compute_rms_per_frame(audio, TARGET_SR, VIDEO_FPS)
    max_rms = np.max(rms) if np.max(rms) > 0.01 else 1.0

    font_large = try_load_font(48)
    font_small = try_load_font(28)

    n_frames = len(rms)
    print(f"  Rendering {n_frames} frames for {name}...")

    for i in range(n_frames):
        img = render_frame(name, color, rms[i], max_rms, i, font_large, font_small)
        img.save(output_dir / f"frame_{i:05d}.jpg", quality=85)

        if (i + 1) % 300 == 0:
            print(f"    {i + 1}/{n_frames} frames")

    print(f"    Done: {n_frames} frames in {output_dir}")


# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

def main():
    OUTPUT_DIR.mkdir(exist_ok=True)

    print("Loading voices...")
    alice_voice = PiperVoice.load(str(VOICES_DIR / "amy-medium.onnx"))
    bob_voice = PiperVoice.load(str(VOICES_DIR / "joe-medium.onnx"))
    native_sr = alice_voice.config.sample_rate  # 22050
    print(f"Native sample rate: {native_sr} Hz, target: {TARGET_SR} Hz")

    alice_segments = []
    bob_segments = []
    pause = silence(PAUSE_MS, TARGET_SR)

    for speaker, text in CONVERSATION:
        voice = alice_voice if speaker == "alice" else bob_voice
        print(f"  [{speaker}] {text}")

        raw = synthesize_line(voice, text)
        resampled = resample(raw, native_sr, TARGET_SR)
        line_silence = np.zeros(len(resampled), dtype=np.float32)

        if speaker == "alice":
            alice_segments.append(resampled)
            bob_segments.append(line_silence)
        else:
            alice_segments.append(line_silence)
            bob_segments.append(resampled)

        alice_segments.append(pause)
        bob_segments.append(pause)

    alice_audio = np.concatenate(alice_segments)
    bob_audio = np.concatenate(bob_segments)

    # Write WAV files
    alice_wav = OUTPUT_DIR / "conversation-alice.wav"
    bob_wav = OUTPUT_DIR / "conversation-bob.wav"
    write_wav(alice_wav, alice_audio, TARGET_SR)
    write_wav(bob_wav, bob_audio, TARGET_SR)

    dur = len(alice_audio) / TARGET_SR
    print(f"\nAudio generated ({dur:.1f}s each):")
    print(f"  {alice_wav}")
    print(f"  {bob_wav}")

    # Generate video frames
    print(f"\nGenerating video frames ({VIDEO_W}x{VIDEO_H} @ {VIDEO_FPS}fps)...")
    generate_frames("Alice", ALICE_COLOR, alice_audio, OUTPUT_DIR / "frames-alice")
    generate_frames("Bob", BOB_COLOR, bob_audio, OUTPUT_DIR / "frames-bob")

    n_frames = len(alice_audio) // (TARGET_SR // VIDEO_FPS)
    print(f"\nDone! {n_frames} frames per bot ({dur:.1f}s × {VIDEO_FPS}fps)")


if __name__ == "__main__":
    main()
