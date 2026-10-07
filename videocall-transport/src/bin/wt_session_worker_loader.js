importScripts("./wt_session_worker.js");
wasm_bindgen("./wt_session_worker_bg.wasm").catch((e) => setTimeout(() => { throw e; }));
