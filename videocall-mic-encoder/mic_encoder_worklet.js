// Mic encoder AudioWorklet. Feeds each render quantum to the ropus encoder
// wasm (src/main.rs) and posts every 20 ms Opus packet as
// { message: "page", page: Uint8Array }, the same messages opus-recorder sent.
class EncoderWorklet extends AudioWorkletProcessor {
  constructor(options) {
    super();
    // processorOptions is the WebAssembly.Module compiled on the main thread.
    // wasm-bindgen leaves one import, its externref table setup, which only
    // __wbindgen_start calls. I never call that, so a no-op is enough. Any
    // other import means a dependency changed the module: fail loudly.
    const module = options.processorOptions;
    const imports = {};
    for (const { module: from, name } of WebAssembly.Module.imports(module)) {
      if (name !== "__wbindgen_init_externref_table") {
        throw new Error(
          `mic encoder wasm has an unexpected import: ${from}.${name}`,
        );
      }
      (imports[from] ??= {})[name] = () => {};
    }
    this.wasm = new WebAssembly.Instance(module, imports).exports;
    this.ready = false;
    this.encoding = false;
    this.alive = true;
    this.port.onmessage = ({ data }) => this.command(data);
  }

  command(data) {
    switch (data.command) {
      case "init":
        this.ready =
          this.wasm.mic_encoder_init(
            data.originalSampleRate ?? sampleRate,
            data.encoderBitRate ?? 50000,
          ) === 0;
        this.encoding = this.ready;
        this.port.postMessage({ message: this.ready ? "ready" : "error" });
        break;
      case "start":
        this.encoding = this.ready;
        break;
      case "stop":
        this.encoding = false;
        break;
      case "flush":
        this.port.postMessage({ message: "flushed" });
        break;
      case "done":
        this.ready = this.encoding = false;
        this.port.postMessage({ message: "done" });
        break;
      case "close":
        this.alive = false;
        break;
    }
  }

  process(inputs) {
    const samples = inputs[0]?.[0];
    if (!this.encoding || !samples?.length) return this.alive;
    const wasm = this.wasm;
    // Any wasm call may grow memory and detach the old buffer, so I make the
    // calls first and only then take wasm.memory.buffer for the view.
    const ptr = wasm.mic_encoder_input(samples.length);
    new Float32Array(wasm.memory.buffer, ptr, samples.length).set(samples);
    const count = wasm.mic_encoder_encode(samples.length);
    for (let i = 0; i < count; i++) {
      const start = wasm.mic_encoder_packet_ptr(i);
      const len = wasm.mic_encoder_packet_len(i);
      const page = new Uint8Array(wasm.memory.buffer, start, len).slice();
      this.port.postMessage({ message: "page", page }, [page.buffer]);
    }
    return this.alive;
  }
}

registerProcessor("encoder-worklet", EncoderWorklet);
