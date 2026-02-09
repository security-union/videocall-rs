/*
 * Copyright 2025 Security Union LLC
 *
 * Licensed under either of
 *
 * * Apache License, Version 2.0
 *   (http://www.apache.org/licenses/LICENSE-2.0)
 * * MIT license
 *   (http://opensource.org/licenses/MIT)
 *
 * at your option.
 */

//! videocall-ui library root.
//!
//! Re-exports public modules so that integration tests (under `tests/`) can
//! import components. The binary entry-point lives in `main.rs`.

#[allow(non_camel_case_types)]
pub mod components;
pub mod constants;
pub mod context;
pub mod types;
