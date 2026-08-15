# Rover brand — generation prompts

The prompts that produced the videocall.rs brand art, archived so every asset
can be regenerated or extended on-model. Pipeline details at the bottom.

## 1. Mission patch (still, `mission-patch.png`)

Model: `gemini-3-pro-image` (Gemini API `generateContent`, IMAGE modality).

> Embroidered military mission patch, circular, photographed straight-on on dark
> worn cotton twill. 1980s sci-fi space-marine squadron insignia style. Palette
> strictly limited to 4 thread colors: charcoal black, bone white, burnt
> rust-orange, olive drab. Arched stitched stencil capitals around the top border
> read "VIDEOCALL.RS"; bottom arc reads "REAL-TIME VIDEO TRANSPORT". Thick
> merrowed overlock edge, visible satin-stitch thread texture, slight wear.
> Utilitarian, grim, functional; 1979-1986 retro-futurism, no gloss, no
> gradients, no chrome. Macro product photo, even lighting, high detail. Square
> 1:1 composition, patch centered and filling the frame. Center emblem: a
> four-legged robotic rover with a single cyclops camera-lens head, standing
> alert on a cratered ridge under a black star field, a small dashed cone of
> video signal rising from its head.

## 2. Flat mascot (still, `rover-mascot.png` / `rover-mascot-black-bg.png`)

Model: `gemini-3-pro-image`, reference image = the mission patch render.
Two steps: extract the character as a flat mascot, then remove the floating
signal cone.

Step 1:

> Using the attached embroidered patch as the character reference: extract the
> four-legged spider rover robot with the single cyclops camera-lens head and
> redraw it as a clean flat vector-style mascot illustration. Same robot, same
> proportions, same personality: boxy armored body, camera-lens eye, four
> articulated spider legs, small dashed cone of video signal rising from its
> head. NO patch, NO fabric, NO embroidery texture, NO circular border, NO text
> anywhere. Limited flat palette: bone white, burnt rust-orange #D96B3C, olive
> drab, warm grey line work. Isolated on a solid pure black #000000 background,
> centered, generous empty margin around it, bold clean shapes with thick
> confident outlines, screen-print / sticker style, 1980s sci-fi utilitarian
> character design. Simplified emblem version: front-on symmetrical pose,
> reduced to the most iconic bold shapes, readable when scaled down to 48
> pixels.

Step 2 (reference = step 1 output):

> Using the attached flat mascot illustration as the exact character reference:
> reproduce this same four-legged spider rover robot mascot EXACTLY as drawn —
> same pose, same proportions, same colors (bone white, burnt rust-orange, olive
> drab, warm grey outlines), same flat sticker style — but REMOVE the floating
> dashed cone/loop shape above its head entirely. Nothing floats above the
> robot; the top of the body is clean. Keep the small antenna nub on the body if
> present. Isolated on a solid pure black #000000 background, centered, generous
> empty margin, no text, no patch, no border.

Background removal for the web asset: flood-fill from the image border through
near-black pixels only (threshold max(R,G,B) <= 55), so interior dark outline
strokes survive; then crop + pad, quantize to a palette PNG.

## 3. Animation clips (VEO)

Model: `veo-3.1-fast-generate-preview` (`:predictLongRunning`), seed image =
`rover-mascot-black-bg.png` passed as BOTH the first frame ("image") and, where
supported, the last frame — so every clip opens and closes on the same pose and
clips cut cleanly into each other. Params: `{"aspectRatio":"16:9",
"durationSeconds":6, "resolution":"720p"}`, then square-crop + VP9 in post.

Shared boilerplate appended to every behavior sentence:

> A flat sticker-illustration robot rover, the exact character in the reference
> image: a boxy olive-green and rust-orange camera head with a single large
> round glossy lens eye, standing on four thin insect-like jointed legs. Solid
> plain matte black background, static locked-off camera, character centered and
> fully in frame. Keep the identical flat illustration style, thick dark
> outlines, and muted olive-green, rust-orange and cream color palette. Cartoon
> squash-and-stretch. The rover BEGINS and ENDS in the exact same neutral
> resting standing pose shown in the reference image, so the animation is a
> seamless loop. No camera movement, no zoom, no pan, no background elements, no
> text, no style drift, keep anatomy consistent.

Kept behaviors (shipped set):

- **idle-wiggle** — happy little body bounce, feet tapping in sequence, head
  tilting curiously side to side, a cute lens-iris blink, a tiny excited hop
  near the middle. (Reconstructed wording; original request not archived.)
- **curious-scan** — slow curious look left and right, lens iris blink at each
  end, one front foot tap. (Reconstructed wording.)
- **happy-hop** — verbatim:
  > The rover starts in its neutral standing pose, then does an excited happy
  > double bounce: it squashes down and springs up into a little hop, lands with
  > its four legs splaying slightly outward, immediately squashes and springs
  > into a second smaller hop, then settles back down into the exact same
  > neutral standing pose it started in. Big joyful bouncy squash-and-stretch,
  > the lens eye wide and delighted.

Rejected behaviors (generated, discarded for artifacts — do not reuse the
outputs, prompts kept for reference): **wake-up** (drowsy droop + puppy shiver),
**little-march** (marching in place).

## 4. Relay globe (`relay-globe.webm`)

Two-stage: style frame with `gemini-3-pro-image`, then VEO with that frame as
seed.

Style frame (second, refined iteration — reference = first iteration):

> Using the attached image as the exact style and composition reference,
> reproduce this same cinematic holographic wireframe dot-matrix Earth globe
> with the SAME dark oxide palette, SAME stippled warm-grey continents, SAME
> thin warm-grey graticule, SAME burnt rust-orange glowing relay nodes and
> glowing arcs, SAME #0A0A0B near-black background, SAME centered square
> framing. Make ONLY these refinements: Every burnt rust-orange (#D96B3C)
> light-arc must begin at one relay node and end at another relay node that are
> BOTH clearly on the visible front face of the globe. NO arcs that trail off
> the edge into empty black space. Keep about 6 or 7 relay nodes on major cities
> across North America, South America, Europe, and West Africa, each with a soft
> warm-orange bloom. Draw about 5 clean graceful arcs that bulge gently outward
> from the sphere surface, each with a small bright traveling highlight, forming
> a tidy connected mesh across the visible face. Continents as fine glowing
> dot-matrix stipple, never solid fill, never photoreal. The globe sits fully
> inside the frame with generous even margin. Premium tech-brand hero visual.
> Moody, high detail, soft edge falloff. NO text, NO labels, NO UI, NO chrome.

Motion (VEO):

> A cinematic holographic dot-matrix Earth globe, centered, on a solid
> near-black background. The camera is completely static and locked off — no
> zoom, no push, no pan, no parallax. Motion, all subtle and continuous and
> seamless: the globe rotates very slowly and steadily on its vertical axis,
> west to east, an almost imperceptible drift. Along the thin burnt rust-orange
> arcs connecting the glowing relay nodes, small bright pulses of orange light
> continuously launch from one node, travel smoothly along the arc, and arrive
> at the other node — like packets of encrypted media flowing across the mesh.
> Each relay node gently pulses and blooms brighter the instant a light-pulse
> arrives at it, then settles. The warm-grey dot-matrix continents and wireframe
> graticule shimmer faintly, a soft holographic flicker. A few extremely sparse
> background stars twinkle very subtly. Mood: premium, moody, high-tech, quiet.
> No text, no labels, no UI, no camera movement.

Loop fix in post (rotation means ends don't match): trim the first second, then
crossfade the head over the tail —
`xfade=transition=fade:duration=1` at `offset = duration - fade`, square-crop,
`libvpx-vp9 -crf 42`.

## Pipeline notes

- Key: `AI_STUDIO` env var (Google AI Studio). Imagen `:predict` models are
  retired for this key — stills go through `gemini-3-pro-image` on
  `generateContent` (n=1 only, no aspect param: put framing in the prompt).
- VEO calls are long-running operations: POST `:predictLongRunning`, poll the
  operation, download the file URI with the key header.
- Harness: `videoeditor-genai` crate (`~/Documents/videoeditor`) for stills;
  plain curl for VEO.

## 5. Relay globe v2 — programmatic perfect loop (shipped)

The VEO globe's rotation never completed, so the loop read as a reset. The
shipped `relay-globe.webm` is instead rendered programmatically by
`relay-globe-render.py` (this directory): Natural Earth 110m land sampled to a
dot-matrix, orthographic projection sweeping exactly one 360° revolution over
the clip, ~18 relay cities (six across the US) with 8-12 arcs visible at any
moment, every pulse phased at an integer multiple of the rotation period —
frame N equals frame 0 by construction. Re-render with
`nix-shell -p "python3.withPackages (p: [p.pillow p.numpy])" ffmpeg` and tweak
the constants at the top of the script (cities, arcs, glow, rotation seconds).
