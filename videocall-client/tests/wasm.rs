use wasm_bindgen_test::*;

// Configure wasm_bindgen_test to use the browser (headless) test runner
wasm_bindgen_test_configure!(run_in_browser);

// This test is a simple sanity check that the WASM test environment works
#[wasm_bindgen_test]
fn wasm_sanity_check() {
    assert!(true);
} 