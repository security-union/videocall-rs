# Diagnostics Drawer Audit

_Date: 2025-XX-XX_

## Current Layout Overview

```
┌───────────────────────────────────────────┐
│ Call Diagnostics               [ × ]      │  ← drawer header (flex)
├───────────────────────────────────────────┤
│ Application Version                       │
│ ────────────────────────────────────────  │
│ Connection Manager                        │
│ ────────────────────────────────────────  │
│ Peer Selection (if >1)                    │
│ …                                         │
│ NetEQ Status                              │
│ NetEQ Charts (grid)                       │
│ …                                         │
└───────────────────────────────────────────┘
```

Key CSS hooks:
* `.sidebar-header` – flex row, contains `<h2>` and close button.
* `.sidebar-content` – vertical stack of `.diagnostics-section` or custom blocks.
* Each section is full-width; there is **no concept of right-hand mini-cards** at the moment.

## Proposed Insertion Point

We want a small *quick-glance* host-system pill aligned to the **top-right** of the drawer header (mobile & desktop).

```
┌───────────────────────────────────────────┐
│ Call Diagnostics     Host ▸      [ × ]    │
└─▲─────────────────────────────────────────┘
  │ click ▸ toggles expand ↓                
```

When expanded the full section becomes the first regular `.diagnostics-section` underneath the version block.

## Styling Constraints
* Header currently uses `justify-content: space-between`. We can insert a flex item (`<SystemSpecsButton>`)
  before the close button.
* Mobile width tested at 320 px: we have ~48 px spare; plan to show an icon **"Host"** or device glyph, truncating UA.
* Use existing palette (`#d1ecf1` light cyan) to differentiate.

## Tech Notes
* Expand state will live in Diagnostics component state (or a reducer) so that only one source of truth controls
  the system-specs panel.
* Accessibility: `aria-expanded` on the button, `aria-controls` referencing the panel id.

## Next Actions
1. Wire-frame & quick-glance field selection (see companion `system_specs_wireframe.md`).
2. Confirm with UX / Lead Engineer.  
3. Begin Stub B implementation once approved.
