# System Specs Panel — Wireframe

## 1. Quick-Glance (collapsed)

```
┌────────────────────────────────────────────┐
│ Call Diagnostics        📟 Host ▸   [ × ]  │
└────────────────────────────────────────────┘
```
* **Icon & label** `📟 Host` occupies ~80 px.
* Shows on all screen sizes.
* Tooltip on hover: *"Click for device details"*.

## 2. Expanded Detail Section

Appears as first `.diagnostics-section` after *Application Version*.

```
┌ Host System ─────────────────────────────┐
│ User Agent:  Mozilla/5.0 … Safari/605.1  │
│ Platform:    iPhone                      │
│ CPU Cores:   6                           │
│ Memory:      4 GB                        │
│ Screen:      390 × 844 @3.0 DPR          │
│ Network:     4g 2.3 Mbps                 │
└───────────────────────────────────────────┘
```

* **Typography** identical to other sections.
* JSON prettified fallback available via tiny *View Raw* toggle.

## Interaction
* Clicking the header *📟 Host ▸* rotates the arrow and scrolls into view if collapsed.
* Esc key closes any expanded sections (consistent with existing drawer behaviour).

## Field Priority List
| Priority | Field | Label | Example |
|----------|-------|-------|---------|
| High | `platform` | Platform | `MacIntel`, `Android` |
| High | `cpu_cores` | CPU | `8` |
| High | `device_memory_gb` | Memory | `16 GB` |
| High | `network_type` | Network | `4g` |
| Med | `screen_width×height @dpr` | Screen | `1920×1080 @2` |
| Med | `user_agent` (truncated) | UA | `Chrome/124` |
| Low | `languages[0]` | Locale | `en-US` |

(Full JSON visible in raw mode.)
