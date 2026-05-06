# Styling Tokens in dioxus-ui (Current System)
This is a practical guide for engineers new to this styling setup. It describes what exists today, where to edit, and how to avoid token drift.

## Where to edit values today

1. CSS token source: `dioxus-ui/static/global.css`
  - Edit token values in the `:root` block.
2. Frozen token contract: `dioxus-ui/static/tokens-v0.json`
  - Contract values are drift-checked against `global.css`.
3. Rust token constants: `dioxus-ui/src/theme.rs`
  - Used for Rust-rendered styles/SVG/chart colors.
4. Usage sites:
  - CSS selectors in `dioxus-ui/static/style.css`
  - Rust components in `dioxus-ui/src/components/*`

## How the architecture works now

1. CSS custom properties in `global.css` drive most surfaces/effects.
  - Examples: `--bg`, `--surface`, `--overlay-strong`, `--effect-blur-sm`.
2. Contract aliases are present for stable semantic names.
  - Examples: `--color-bg`, `--color-surface`, `--status-success`.
3. Rust keeps a separate token set in `theme.rs`.
  - Examples: `SIGNAL_AUDIO`, `SIGNAL_VIDEO`, `SIGNAL_LATENCY_DIM`.
4. `style.css` still contains many literal spacing values.
  - Spacing tokens exist, but not all selectors are wired to them yet.

## Real examples (before/after)

### 1) Change app background / surface color

Edit `dioxus-ui/static/global.css`:

```css
/* before */
:root {
  --bg: #000000;
  --surface: #1C1C1E;
  --surface-elevated: #2C2C2E;
}

/* after */
:root {
  --bg: #050506;
  --surface: #222225;
  --surface-elevated: #2f3034;
}
```

Existing consumers include:

```css
html { background-color: var(--bg); }
body { background-color: var(--bg); }
.search-modal-card { background: var(--color-surface); }
```

### 2) Adjust spacing scale and where it affects UI
Current reality: changing `--space-*` may not affect much until selectors use those vars.

Edit scale in `global.css`:

```css
/* before */
:root {
  --space-3: 0.75rem;
  --space-4: 1rem;
  --space-5: 1.25rem;
}

/* after */
:root {
  --space-3: 0.625rem;
  --space-4: 0.875rem;
  --space-5: 1.125rem;
}
```

Wire a real selector in `style.css`:

```css
/* before */
.search-modal-badge {
  margin-left: 12px;
  padding: 3px 10px;
}

/* after */
.search-modal-badge {
  margin-left: var(--space-3);
  padding: var(--space-1) var(--space-3);
}
```

This directly affects search row badge chip spacing.

### 3) Change modal blur/overlay strength

`style.css` already uses tokens:

```css
.search-modal-overlay {
  background: var(--overlay-strong);
  backdrop-filter: blur(var(--effect-blur-sm));
}
```

Tune in `global.css`:

```css
/* before */
:root {
  --overlay-strong: rgba(0, 0, 0, 0.5);
  --effect-blur-sm: 4px;
}

/* after */
:root {
  --overlay-strong: rgba(0, 0, 0, 0.62);
  --effect-blur-sm: 6px;
}
```

### 4) Change search badge colors

`style.css` badge variants:

```css
.search-modal-badge-active { background: var(--search-badge-active-bg); color: var(--search-badge-active-text); }
.search-modal-badge-ended { background: var(--search-badge-ended-bg); color: var(--search-badge-ended-text); }
.search-modal-badge-idle { background: var(--search-badge-idle-bg); color: var(--search-badge-idle-text); }
```

Edit token values in `global.css`:

```css
/* before */
:root {
  --search-badge-active-bg: rgba(22, 163, 74, 0.15);
  --search-badge-active-text: #4ade80;
  --search-badge-ended-bg: rgba(75, 85, 99, 0.2);
  --search-badge-ended-text: #9ca3af;
  --search-badge-idle-bg: rgba(202, 138, 4, 0.15);
  --search-badge-idle-text: #facc15;
}

/* after */
:root {
  --search-badge-active-bg: rgba(34, 197, 94, 0.12);
  --search-badge-active-text: #86efac;
  --search-badge-ended-bg: rgba(107, 114, 128, 0.16);
  --search-badge-ended-text: #cbd5e1;
  --search-badge-idle-bg: rgba(234, 179, 8, 0.12);
  --search-badge-idle-text: #fde68a;
}
```

### 5) Change signal chart colors via Rust tokens

Edit `dioxus-ui/src/theme.rs`:

```rust
/* before */
pub const SIGNAL_AUDIO: &str = "#4FC3F7";
pub const SIGNAL_VIDEO: &str = "#81C784";
pub const SIGNAL_SCREEN: &str = "#CE93D8";
pub const SIGNAL_LATENCY: &str = "#FF8A65";
pub const SIGNAL_LATENCY_DIM: &str = "rgba(255,138,101,0.4)";

/* after */
pub const SIGNAL_AUDIO: &str = "#67E8F9";
pub const SIGNAL_VIDEO: &str = "#86EFAC";
pub const SIGNAL_SCREEN: &str = "#C4B5FD";
pub const SIGNAL_LATENCY: &str = "#FDBA74";
pub const SIGNAL_LATENCY_DIM: &str = "rgba(253,186,116,0.4)";
```

Used in `dioxus-ui/src/components/signal_quality.rs` (chart lines + legend dots).

## Safe workflow checklist

1. Edit source-of-truth files (`global.css`, `tokens-v0.json`, `theme.rs`).
2. Run:

```bash
make check-style-tokens
```

3. If drift check fails (`check-token-drift.sh`):
  - Fix `MISSING`/`DRIFT` by aligning token name/value in both:
  - `dioxus-ui/static/global.css`
  - `dioxus-ui/static/tokens-v0.json`
4. If hardcoded color check fails (`check-hardcoded-colors.sh`):
  - Move new literals from non-token files into token files.
  - Replace literals with token references.
5. Re-run `make check-style-tokens`.

## Add new token or reuse existing?

Reuse existing token when:
- The semantic meaning already matches.
- You want consistency across multiple components.

Add a new token when:
- The value has a distinct semantic role.
- It will be reused, or should be contract-tracked.

Avoid new tokens for one-off experiments and external brand colors.

## Troubleshooting

1. Changed `global.css` but no visible change.
  - The selector may still use a literal in `style.css` or Rust inline styles.
2. Drift check fails after token edit.
  - `tokens-v0.json` and `global.css` values are out of sync.
3. Chart color did not change after CSS edit.
  - Signal chart colors come from `theme.rs`, not CSS vars.
4. CI flags hardcoded colors.
  - A new literal was added outside allowlisted token files.
5. Spacing scale edit has little impact.
  - Many selectors still use literal spacing; migrate them to `var(--space-*)`.
