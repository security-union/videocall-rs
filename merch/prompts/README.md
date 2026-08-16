# Prompt archive — every generation, verbatim

Raw prompt files exactly as sent to the models, organized by asset family and
round. Narrative pipeline docs live in `../PROMPTS.md`; this directory is the
byte-accurate record. Policy: prompts are archived at generation time and never
overwritten or deleted.

Models: stills = `gemini-3-pro-image` (Gemini `generateContent`, IMAGE
modality); video = `veo-3.1-fast-generate-preview` (`:predictLongRunning`).
"Pinned" = the same seed image passed as first frame and last frame.

## patch/ — embroidered mission-patch stills

| File | Status | Notes |
|---|---|---|
| c-rover.txt | **SHIPPED** (became the brand) | Quadruped camera rover on a ridge; source of the mascot |
| c-drone.txt | rejected | Recon drone concept |
| c-robohead.txt | rejected | 80s robot head concept |
| c-lenseye.txt | rejected | Lens-eye reticle concept |

Round-1 antenna concepts (4 patches) predate the archive policy; their shared
style block is preserved in ../PROMPTS.md §1.

## mascot/ — flat mascot stills (reference = patch render)

| File | Status | Notes |
|---|---|---|
| m-standing.txt | superseded | Three-quarter stance w/ signal cone |
| m-walking.txt | rejected | Mid-step sticker scene |
| m-emblem.txt | intermediate | Front-on emblem, had the floating cone |
| m-emblem2.txt | **SHIPPED** | Cone removal pass (reference = m-emblem output) |

Mascot VEO clips (idle-wiggle, curious-scan, happy-hop; rejected wake-up,
little-march): verbatim + reconstructed prompts in ../PROMPTS.md §3.

## globe/ — relay globe (VEO route, superseded)

| File | Status | Notes |
|---|---|---|
| style-1.txt | intermediate | First hologram-globe style frame |
| style-2.txt | intermediate | Refinement pass (reference = style-1 output) |
| motion-1.txt | superseded | VEO motion; rotation never completed, loop read as a reset |

The SHIPPED globe is programmatic (`../relay-globe-render.py`), not prompted.

## fleet/ — Mars/Moon control-room feeds

| File | Status | Notes |
|---|---|---|
| round1-style-ground.txt | seed | Shared Mars style frame (ground feeds) |
| round1-style-aerial.txt / -aerial2.txt | seed | Aerial style frames |
| round2-unit-02-fpv.txt | superseded | Forward crawl, pinned — rubber-banded |
| round2-unit-03-aerial.txt | **SHIPPED** | Tracking drone, pinned |
| round2-unit-04-orbiter.txt | superseded | First lunar orbiter, pinned — froze mid-clip |
| round3-style-orbiter.txt | seed | Lunar orbiter style frame |
| round3-unit-02-fpv-parked.txt | **SHIPPED** | Parked-rover physics fix, pinned |
| round3-unit-04-orbiter.txt | **SHIPPED — FLAGGED** | Owner: serious artifacts on the satellite. Root cause hypothesis: rigid-body companion sat forced through a cyclical arc + pinned endpoints + "constantly flexing" foil = structural morphing. Round 4 pending. |

Round-1 VEO motion prompts (crossfade loops) were overwritten by round 2
before the archive policy existed; the clips themselves survive in
`../clips/fleet-v1-crossfade/`.

## Conventions for future rounds

1. Write the prompt to `fleet/roundN-<unit>-<slug>.txt` BEFORE generating.
2. Record params + conditioning in the table above with status.
3. Never overwrite: new round = new files; superseded outputs move to
   `../clips/`.
