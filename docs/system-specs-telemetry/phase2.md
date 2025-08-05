# Phase 2 — Transport & Backend Integration

**Objective:** Deliver the collected `SystemSpecs` to the backend so it is stored alongside participant diagnostics.

---

## Stub A — Protobuf Schema Update  
*(Owner: Protocol engineer)*

| Serial | Task | Notes |
|--------|------|-------|
| **2-A-1** | Add `SystemSpecs` message to `protobuf/types/connection_packet.proto`. | choose next free tag |
| **2-A-2** | Regenerate Rust code via `prost` (`make proto`). |  |
| **2-A-3** | Bump crate version for `videocall-types`. |  |
| **2-A-4** | **PAUSE – compile & unit tests** |  |

---

## Stub B — Client Transmission  
*(Owner: FE/Networking)*

| Serial | Task | Notes |
|--------|------|-------|
| **2-B-1** | Update `videocall-client` connection handshake builder to attach `SystemSpecs`. | `src/connection/connection.rs` |
| **2-B-2** | Optionally emit a one-shot `DiagEvent` (`subsystem = "system_specs"`). | leverage existing diagnostics channel |
| **2-B-3** | **PAUSE – Web console verify outbound frame** |  |

---

## Stub C — Backend Actix Handling  
*(Owner: BE engineer)*

| Serial | Task | Notes |
|--------|------|-------|
| **2-C-1** | Expand `ConnectionRequest` handler to parse/store `specs`. | file `actix-api/src/handlers/*.rs` |
| **2-C-2** | Persist to MongoDB `participants` collection. | new optional field `system_specs` |
| **2-C-3** | Add admin API endpoint `/diagnostics/specs/:meeting/:participant`. | for support dashboards |
| **2-C-4** | **PAUSE – integration test with mock client** |  |

---

## Stub D — Metrics & Logging  
*(Owner: DevOps)*

| Serial | Task | Notes |
|--------|------|-------|
| **2-D-1** | Update Kibana/Grafana dashboards to include specs fields. |  |
| **2-D-2** | Create alert rule for "device_memory_gb < 2" combined with high jitter. |  |
| **2-D-3** | **PAUSE – user review of dashboards** |  |
