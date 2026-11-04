#!/usr/bin/env python3
"""Writes assets/clock-voices/<participant>/<clip>.ogg:
  nNN  cardinal number words ("zero" .. "fifty-nine"): hours and seconds
  mNN  minutes 00-09 as read on a clock ("hundred", "oh one" .. "oh nine")
  b-*  listener backchannels; f-*  speaker fillers

Usage:
  python3 generate-clock-voices.py                 # alice bob carol dave
  python3 generate-clock-voices.py alice eve       # named participants
  python3 generate-clock-voices.py --all           # every participant below
"""

import asyncio
import subprocess
import sys
import tempfile
from pathlib import Path

import edge_tts

OUTPUT_DIR = Path(__file__).resolve().parent.parent / "assets" / "clock-voices"
DEFAULT_PARTICIPANTS = ["alice", "bob", "carol", "dave"]
OPUS_BITRATE = "24k"
ATTEMPTS = 4

VOICE_MAP = {
    "alice": "en-US-AvaNeural",
    "bob": "en-US-AndrewNeural",
    "carol": "en-US-EmmaNeural",
    "dave": "en-US-BrianNeural",
    "eve": "en-US-JennyNeural",
    "frank": "en-US-GuyNeural",
    "grace": "en-GB-SoniaNeural",
    "henry": "en-GB-RyanNeural",
    "iris": "en-AU-NatashaNeural",
    "jack": "en-US-ChristopherNeural",
    "karen": "en-US-MichelleNeural",
    "leo": "en-GB-ThomasNeural",
    "mona": "en-US-AriaNeural",
    "nick": "en-IE-ConnorNeural",
    "olivia": "en-AU-NatashaNeural",
    "pete": "en-CA-LiamNeural",
    "quinn": "en-CA-ClaraNeural",
    "rosa": "en-IN-NeerjaNeural",
    "sam": "en-IN-PrabhatNeural",
    "tina": "en-GB-LibbyNeural",
}

SPOKEN_EXTRAS = {
    "b-mmhm": "Mm-hmm.",
    "b-uhhuh": "Uh-huh.",
    "b-yeah": "Yeah.",
    "b-right": "Right.",
    "b-okay": "Okay.",
    "b-sure": "Sure.",
    "b-gotit": "Got it.",
    "b-exactly": "Exactly.",
    "f-um": "Um...",
    "f-uh": "Uh...",
}

ONES = ["zero", "one", "two", "three", "four", "five", "six", "seven", "eight", "nine",
        "ten", "eleven", "twelve", "thirteen", "fourteen", "fifteen", "sixteen",
        "seventeen", "eighteen", "nineteen"]
TENS = {20: "twenty", 30: "thirty", 40: "forty", 50: "fifty"}


def cardinal(n: int) -> str:
    if n < 20:
        return ONES[n]
    tens, ones = divmod(n, 10)
    word = TENS[tens * 10]
    return word if ones == 0 else f"{word}-{ONES[ones]}"


def clip_texts() -> dict[str, str]:
    texts = {f"n{n:02d}": cardinal(n) for n in range(60)}
    texts["m00"] = "hundred"
    texts.update({f"m{n:02d}": f"oh {ONES[n]}" for n in range(1, 10)})
    texts.update(SPOKEN_EXTRAS)
    return texts


async def fetch_mp3(voice: str, text: str, path: str) -> None:
    for attempt in range(1, ATTEMPTS + 1):
        try:
            await edge_tts.Communicate(text, voice).save(path)
            return
        except edge_tts.exceptions.EdgeTTSException:
            if attempt == ATTEMPTS:
                raise
            await asyncio.sleep(2 * attempt)


async def synthesize(voice: str, text: str, out: Path) -> None:
    with tempfile.NamedTemporaryFile(suffix=".mp3") as mp3:
        await fetch_mp3(voice, text, mp3.name)
        trim = "silenceremove=start_periods=1:start_threshold=-50dB:start_silence=0.02"
        subprocess.run(
            ["ffmpeg", "-hide_banner", "-loglevel", "error", "-y", "-i", mp3.name,
             "-af", f"{trim},areverse,{trim},areverse",
             "-ac", "1", "-ar", "48000", "-c:a", "libopus", "-b:a", OPUS_BITRATE,
             "-application", "voip", "-map_metadata", "-1",
             "-fflags", "+bitexact", "-flags:a", "+bitexact", str(out)],
            check=True,
        )


async def generate(participant: str) -> None:
    voice = VOICE_MAP[participant]
    out_dir = OUTPUT_DIR / participant
    out_dir.mkdir(parents=True, exist_ok=True)
    for name, text in clip_texts().items():
        out = out_dir / f"{name}.ogg"
        if not out.exists():
            await synthesize(voice, text, out)
    total = sum(p.stat().st_size for p in out_dir.glob("*.ogg"))
    print(f"{participant} ({voice}): {total} bytes")


def main(argv: list[str]) -> int:
    if argv == ["--all"]:
        participants = list(VOICE_MAP)
    else:
        participants = argv or DEFAULT_PARTICIPANTS
    unknown = [p for p in participants if p not in VOICE_MAP]
    if unknown:
        print(f"unknown participant(s): {', '.join(unknown)}", file=sys.stderr)
        return 2
    for participant in participants:
        asyncio.run(generate(participant))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
