#!/usr/bin/env python3
"""Generate per-line conversation audio clips and a manifest for the bot.

Uses Microsoft Edge TTS neural voices. Requires internet for TTS generation;
the resulting WAV clips are then used offline by the bot.

Output structure:
  conversation/
    manifest.yaml       — participant roster + line metadata
    lines/
      line_000.wav      — individual speech clips
      line_001.wav
      ...

The bot reads the manifest at startup, takes the first N participants
(via --users N), filters lines to active speakers, and stitches audio
in memory — so one generation serves any participant count.

Dependencies:
  pip install edge-tts numpy scipy
  apt install ffmpeg  (for mp3 -> wav conversion)

Usage:
  python3 generate-conversation-edge.py                # generate all lines
"""

import asyncio
import io
import subprocess
import sys
import tempfile
import wave

import numpy as np
from pathlib import Path

import edge_tts

OUTPUT_DIR = Path(__file__).parent / "conversation"
TARGET_SR = 48000   # Opus encoder expects 48kHz

# Participants in order — bot takes the first N.
# Run `edge-tts --list-voices | grep en` to see all options.
PARTICIPANTS = [
    {"name": "alice",   "voice": "en-US-AvaNeural",          "ekg_color": [0, 200, 220]},
    {"name": "bob",     "voice": "en-US-AndrewNeural",       "ekg_color": [80, 220, 80]},
    {"name": "carol",   "voice": "en-US-EmmaNeural",         "ekg_color": [220, 160, 40]},
    {"name": "dave",    "voice": "en-US-BrianNeural",        "ekg_color": [200, 80, 200]},
    {"name": "eve",     "voice": "en-US-JennyNeural",        "ekg_color": [220, 80, 80]},
    {"name": "frank",   "voice": "en-US-GuyNeural",          "ekg_color": [80, 180, 220]},
    {"name": "grace",   "voice": "en-GB-SoniaNeural",        "ekg_color": [220, 120, 180]},
    {"name": "henry",   "voice": "en-GB-RyanNeural",         "ekg_color": [140, 220, 140]},
    {"name": "iris",    "voice": "en-AU-NatashaNeural",      "ekg_color": [220, 200, 80]},
    {"name": "jack",    "voice": "en-US-ChristopherNeural",  "ekg_color": [180, 140, 100]},
    {"name": "karen",   "voice": "en-US-MichelleNeural",     "ekg_color": [100, 180, 200]},
    {"name": "leo",     "voice": "en-GB-ThomasNeural",       "ekg_color": [200, 160, 120]},
    {"name": "mona",    "voice": "en-US-AriaNeural",         "ekg_color": [160, 100, 220]},
    {"name": "nick",    "voice": "en-IE-ConnorNeural",       "ekg_color": [120, 200, 160]},
    {"name": "olivia",  "voice": "en-AU-NatashaNeural",      "ekg_color": [220, 140, 100]},
    {"name": "pete",    "voice": "en-CA-LiamNeural",         "ekg_color": [100, 140, 220]},
    {"name": "quinn",   "voice": "en-CA-ClaraNeural",        "ekg_color": [220, 100, 160]},
    {"name": "rosa",    "voice": "en-IN-NeerjaNeural",       "ekg_color": [180, 220, 100]},
    {"name": "sam",     "voice": "en-IN-PrabhatNeural",      "ekg_color": [140, 100, 180]},
    {"name": "tina",    "voice": "en-GB-LibbyNeural",        "ekg_color": [220, 180, 140]},
]

# Voice lookup by name
VOICE_MAP = {p["name"]: p["voice"] for p in PARTICIPANTS}

# 20-participant conversation — each line is self-contained so dropping any
# speaker's lines still produces a coherent conversation.
CONVERSATION = [
    ("alice",  "Hey everyone! Thanks for joining. Let's go around and share some fun facts today."),
    ("bob",    "I'll kick things off. Did you know honey never spoils? They found 3000 year old honey in Egyptian tombs and it was still edible."),
    ("carol",  "That's amazing! Here's one. Octopuses have three hearts, blue blood, and each arm has its own mini brain."),
    ("dave",   "A group of flamingos is called a flamboyance. Best collective noun in the English language."),
    ("eve",    "Bananas are technically berries, but strawberries aren't. Blame botanical definitions."),
    ("frank",  "There are more trees on Earth than stars in the Milky Way. About 3 trillion trees versus 400 billion stars."),
    ("grace",  "The shortest war in history lasted 38 minutes. It was between Britain and Zanzibar in 1896."),
    ("henry",  "Oxford University is older than the Aztec Empire. Oxford started teaching in 1096."),
    ("iris",   "Cleopatra lived closer in time to the moon landing than to the building of the Great Pyramid."),
    ("jack",   "The inventor of the Pringles can is buried in one. His family honored his request."),
    ("karen",  "A day on Venus is longer than a year on Venus. It rotates so slowly that its year finishes first."),
    ("leo",    "The entire world's population could fit inside Los Angeles if they stood side by side."),
    ("mona",   "There are more ways to arrange a deck of cards than atoms on Earth. 52 factorial is mind boggling."),
    ("nick",   "In Switzerland it's illegal to own just one guinea pig. They get lonely so you need at least two."),
    ("olivia", "A jiffy is an actual unit of time. It's one hundredth of a second."),
    ("pete",   "The human nose can detect over one trillion different scents. Way more than we have words for."),
    ("quinn",  "A bolt of lightning is five times hotter than the surface of the sun. About 30,000 Kelvin."),
    ("rosa",   "Sea otters hold hands while sleeping so they don't drift apart. They call it a raft."),
    ("sam",    "Crows can recognize human faces and hold grudges. They'll even tell other crows about you."),
    ("tina",   "Wombat poop is cube-shaped. It helps them mark their territory without the droppings rolling away."),
    ("alice",  "These are all fantastic! OK round two. Anyone have more?"),
    ("frank",  "Neutron stars are so dense that a teaspoon would weigh about 6 billion tons."),
    ("bob",    "Scotland's national animal is the unicorn. They've used it as a symbol since the 12th century."),
    ("iris",   "The longest hiccupping spree lasted 68 years. Charles Osborne hiccupped from 1922 to 1990."),
    ("grace",  "Oh that's terrible! Here's a nicer one. Butterflies taste with their feet."),
    ("dave",   "If you shuffled a deck of cards every second since the Big Bang, you still wouldn't have repeated an order."),
    ("henry",  "Alaska is simultaneously the most northern, western, and eastern US state because of the Aleutian Islands."),
    ("carol",  "The fingerprints of a koala are so similar to humans that they've confused crime scene investigators."),
    ("karen",  "The inventor of the fire hydrant is unknown because the patent was destroyed in a fire."),
    ("jack",   "A group of porcupines is called a prickle. Almost as good as a flamboyance of flamingos."),
    ("leo",    "Sloths can hold their breath longer than dolphins. Up to 40 minutes."),
    ("mona",   "Honey bees can be trained to detect explosives. Their sense of smell is remarkably precise."),
    ("pete",   "There's a species of jellyfish that is biologically immortal. It can revert to its juvenile form."),
    ("nick",   "The dot over the letters i and j is called a tittle. Now you know."),
    ("olivia", "A cloud can weigh more than a million pounds. All that water vapor adds up."),
    ("quinn",  "Vending machines are statistically more dangerous than sharks. They cause more injuries per year."),
    ("rosa",   "Octopuses have been observed using coconut shells as portable shelters. Tool use in invertebrates!"),
    ("sam",    "The longest place name in the world is in New Zealand. It has 85 letters and starts with Taumata."),
    ("tina",   "A group of cats is called a clowder. And a group of kittens is a kindle."),
    ("eve",    "Your brain uses about 20 percent of your total energy despite being only 2 percent of your body weight."),
    ("alice",  "This has been incredible. Same time next week everyone?"),
    ("bob",    "Absolutely! Great session."),
    ("carol",  "Count me in!"),
    ("dave",   "Wouldn't miss it."),
    ("eve",    "See you all then!"),
    ("frank",  "Looking forward to it."),
    ("grace",  "Cheers everyone!"),
    ("henry",  "Take care all."),
    ("iris",   "Bye everyone!"),
    ("jack",   "Later!"),
    ("karen",  "Goodbye!"),
    ("leo",    "See you next time!"),
    ("mona",   "Bye bye!"),
    ("nick",   "Talk soon!"),
    ("olivia", "Bye!"),
    ("pete",   "Peace out!"),
    ("quinn",  "Until next time!"),
    ("rosa",   "Adios!"),
    ("sam",    "Catch you later!"),
    ("tina",   "Bye everyone, this was fun!"),
]


async def synthesize_line(voice: str, text: str) -> np.ndarray:
    """Synthesize a line of text using Edge TTS, return as float32 numpy array at TARGET_SR."""
    comm = edge_tts.Communicate(text, voice)

    mp3_data = b""
    async for chunk in comm.stream():
        if chunk["type"] == "audio":
            mp3_data += chunk["data"]

    # Convert MP3 to 48kHz mono WAV via ffmpeg
    with tempfile.NamedTemporaryFile(suffix=".mp3", delete=True) as mp3_file:
        mp3_file.write(mp3_data)
        mp3_file.flush()

        result = subprocess.run(
            [
                "ffmpeg", "-y", "-i", mp3_file.name,
                "-ar", str(TARGET_SR), "-ac", "1",
                "-f", "wav", "-"
            ],
            capture_output=True,
        )
        if result.returncode != 0:
            raise RuntimeError(f"ffmpeg failed: {result.stderr.decode()}")

    wav_buf = io.BytesIO(result.stdout)
    with wave.open(wav_buf, "rb") as wf:
        n_frames = wf.getnframes()
        raw = wf.readframes(n_frames)
        audio = np.frombuffer(raw, dtype=np.int16).astype(np.float32) / 32768.0

    return audio


def write_wav(path: Path, audio: np.ndarray, sample_rate: int):
    int16_audio = np.clip(audio * 32767, -32768, 32767).astype(np.int16)
    with wave.open(str(path), "wb") as wf:
        wf.setnchannels(1)
        wf.setsampwidth(2)
        wf.setframerate(sample_rate)
        wf.writeframes(int16_audio.tobytes())


async def main():
    print(f"Generating {len(CONVERSATION)} conversation lines for {len(PARTICIPANTS)} participants")

    OUTPUT_DIR.mkdir(exist_ok=True)
    lines_dir = OUTPUT_DIR / "lines"
    lines_dir.mkdir(exist_ok=True)

    manifest_lines = []

    for i, (speaker, text) in enumerate(CONVERSATION):
        voice = VOICE_MAP[speaker]
        print(f"  [{i:03d}] {speaker}: {text[:60]}...")

        try:
            audio = await synthesize_line(voice, text)
        except Exception as e:
            print(f"        ERROR: voice '{voice}' failed: {e}")
            print(f"        Run: edge-tts --list-voices | grep en-")
            sys.exit(1)

        wav_filename = f"lines/line_{i:03d}.wav"
        wav_path = OUTPUT_DIR / wav_filename
        write_wav(wav_path, audio, TARGET_SR)

        duration_ms = int(len(audio) * 1000 / TARGET_SR)

        manifest_lines.append({
            "speaker": speaker,
            "audio_file": wav_filename,
            "duration_ms": duration_ms,
        })

        print(f"        -> {wav_filename} ({duration_ms}ms)")

    # Write manifest
    import yaml
    manifest = {
        "participants": PARTICIPANTS,
        "pause_ms": 800,
        "lines": manifest_lines,
    }

    manifest_path = OUTPUT_DIR / "manifest.yaml"
    with open(manifest_path, "w") as f:
        yaml.dump(manifest, f, default_flow_style=False, sort_keys=False)

    total_speech_ms = sum(l["duration_ms"] for l in manifest_lines)
    total_pause_ms = len(manifest_lines) * 800
    total_ms = total_speech_ms + total_pause_ms
    print(f"\nDone! {len(manifest_lines)} lines, {total_ms / 1000:.1f}s total")
    print(f"  Speech: {total_speech_ms / 1000:.1f}s, Pauses: {total_pause_ms / 1000:.1f}s")
    print(f"  Manifest: {manifest_path}")


if __name__ == "__main__":
    asyncio.run(main())
