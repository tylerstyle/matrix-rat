import init, * as bindings from '/matrix-rat/matrix-rat-web-spike-e551e80b13243f99.js';
const wasm = await init({ module_or_path: '/matrix-rat/matrix-rat-web-spike-e551e80b13243f99_bg.wasm' });


window.wasmBindings = bindings;


dispatchEvent(new CustomEvent("TrunkApplicationStarted", {detail: {wasm}}));
