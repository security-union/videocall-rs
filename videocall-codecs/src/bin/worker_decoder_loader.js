importScripts("./worker_decoder.js");
wasm_bindgen("./worker_decoder_bg.wasm").catch((e) => setTimeout(() => { throw e; }));
