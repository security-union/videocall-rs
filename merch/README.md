# videocall.rs merch assets

Brand artwork for physical production (patches, shirts, stickers). The website
uses its own optimized copies under `leptos-website/public/images/`; these are
the production masters.

## Files

| File | What | Use |
|---|---|---|
| `mission-patch.png` | Embroidered mission-patch design, 1024x1024 photo-style render | Reference for a patch manufacturer |
| `rover-mascot.png` | The rover mascot, flat illustration, transparent background, 828x774 | Shirts, stickers, print on any color |
| `rover-mascot-black-bg.png` | Same mascot on solid black, 1024x1024 | Dark-garment printing reference |

## The mascot

The videocall.rs mascot is a four-legged spider rover with a camera head —
a robot streaming video, which is what the platform does. Style: flat
screen-print illustration, 1980s sci-fi utilitarian.

## Production notes (patch)

- Circular, suggested 3.5" (89 mm) diameter, merrowed overlock edge
- 4 thread colors: charcoal black, bone white, burnt rust-orange
  (approx. Pantone 7526 C / hex `#D96B3C`), olive drab
- Text: "VIDEOCALL.RS" arched top, "REAL-TIME VIDEO TRANSPORT" arched bottom
- Backing: iron-on or hook-and-loop, vendor's choice

## Regenerating / new variants

Generated with `gemini-3-pro-image` via the `videoeditor-genai` crate
(`~/Documents/videoeditor`), using the patch render as the character
reference image so the rover stays on-model. Keep the palette to the four
colors above and the 1979-1986 retro-futurist tone: no gloss, no gradients,
no chrome.
