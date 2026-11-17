# Test audio

`jfk_rice_moon_speech_20s.wav` is the one speech clip every NetEq DTX scenario uses.
It is 20 seconds of President Kennedy's address at Rice University on
September 12, 1962, the "We choose to go to the Moon" speech. 48 kHz, mono, 16 bit.

I picked a stretch with about ten natural pauses of 0.4 s or more, because pauses
are where DTX does its work. The recording has an audible noise floor (tape hiss
and a stadium), so the scenarios that need guaranteed silence build it themselves
from quiet room noise.

## Where it comes from

- Source: [Jfk rice university we choose to go to the moon.ogg](https://commons.wikimedia.org/wiki/File:Jfk_rice_university_we_choose_to_go_to_the_moon.ogg) on Wikimedia Commons
- Credit: John F. Kennedy Presidential Library & Museum
- License: public domain
- Source file SHA-256: `7486fba805c20bde4d43750bd9c34aee86ab7acfd3854a1d0f3d64f62ae68b27`
- Clip SHA-256: `f6bb05587ed403eb4014f14a9986f45ac7d4d144bdd6f8f2b6d21ef81b953e1a`

## How it was cut

Seconds 204 to 224 of the source, downmixed and resampled:

```bash
ffmpeg -ss 204 -t 20 -i Jfk_rice_university_we_choose_to_go_to_the_moon.ogg \
  -ac 1 -ar 48000 -c:a pcm_s16le -map_metadata -1 \
  -fflags +bitexact -flags:a +bitexact jfk_rice_moon_speech_20s.wav
```
