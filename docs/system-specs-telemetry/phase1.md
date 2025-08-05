# Phase 1 — Front-End UX & Data Collection

**Objective:** Provide an intuitive, mobile-friendly "Host System" section inside the *Call Diagnostics* drawer that shows a quick-glance summary and an expandable detailed view. All work remains client-side.

---

## Stub A — UX / Visual Design  
*(Owner: FE/Design)*

| Serial | Task | Notes |
|--------|------|-------|
| **1-A-1** | Audit existing Diagnostics drawer (layout, CSS grid break-points). | code: `yew-ui/src/components/diagnostics.rs` |
| **1-A-2** | Produce wire-frames / design mock-ups (quick glance vs expanded). | e.g. Figma or Markdown sketch placed in `docs/system-specs-telemetry/notes/` |
| **1-A-3** | Define content hierarchy (which fields appear in quick glance). |   |
| **1-A-4** | Review mock-ups with Lead Engineer & UX (feedback). | **PAUSE – user test** |

---

## Stub B — Component Scaffolding & Data Hook  
*(Owner: FE engineer)*

| Serial | Task | Notes |
|--------|------|-------|
| **1-B-1** | Create `<SystemSpecsPanel>` Yew component skeleton with expand/collapse state. | new file `yew-ui/src/components/system_specs_panel.rs` |
| **1-B-2** | Implement memoised `gather_system_specs()` (existing helper) with `use_memo` caching. | ensure only 1 call per session |
| **1-B-3** | Render *quick glance* summary (UA, platform, cores, memory, net type). | fits 2 × rows max |
| **1-B-4** | Render expanded JSON pretty view identical to current prototype. | reuse `serde_json::to_string_pretty` |
| **1-B-5** | Integrate panel into Diagnostics drawer (top-right slot). | update `diagnostics.rs` |
| **1-B-6** | **PAUSE – unit tests & dev manual check** |  |

---

## Stub C — Styling & Accessibility  
*(Owner: FE engineer)*

| Serial | Task | Notes |
|--------|------|-------|
| **1-C-1** | Create SCSS/CSS matching existing design tokens. | colours, border-radius |
| **1-C-2** | Ensure responsive behaviour on <= 375 px wide screens. | grid / flex tweaks |
| **1-C-3** | Add keyboard & screen-reader support for expand button. | `aria-expanded` etc. |
| **1-C-4** | **PAUSE – user acceptance test on mobile & desktop** |  |

---

## Stub D — Automated Front-End Tests  
*(Owner: QA)

| Serial | Task | Notes |
|--------|------|-------|
| **1-D-1** | Write Playwright test: quick glance visible by default. | file `playwright/tests/system-specs.spec.ts` |
| **1-D-2** | Test expand/collapse toggle updates aria attribute & content. |   |
| **1-D-3** | Snapshot test for detailed JSON section. |   |
| **1-D-4** | Wire to CI workflow `wasm-test.yaml`. |   |
| **1-D-5** | **PAUSE – user test run-through & sign-off** |  |
