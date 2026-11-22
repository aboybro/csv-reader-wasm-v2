// csv-wasm solve.js: a WASM SIMD reader that answers exactly what csv-reader
// 1.0.0 answers for an input {text, header}: {columns, rows, errors}.
//
// The heavy work lives in parser.wasm (Rust, v128 SIMD): it scans the input
// once as UTF-8 bytes, builds the answer as JSON text in its own memory, and
// this module decodes that text and hands the parsed answer back. The wasm
// module is instantiated synchronously at load, so run() stays synchronous.

import { readFileSync } from 'node:fs';

const wasmBytes = readFileSync(new URL('./parser.wasm', import.meta.url));
const wasmModule = new WebAssembly.Module(wasmBytes);
const instance = new WebAssembly.Instance(wasmModule, {});
const { memory, alloc_input, parse } = instance.exports;

const encoder = new TextEncoder();
const decoder = new TextDecoder();

let mem = new Uint8Array(memory.buffer);
function refreshMem() {
  if (mem.buffer !== memory.buffer) mem = new Uint8Array(memory.buffer);
}

/** csv-reader 1.0.0's answer for one case's input. */
export function run(input) {
  const text = String(input.text);
  const header = input.header === true;
  const bytes = encoder.encode(text);
  const ptr = alloc_input(bytes.length);
  refreshMem();
  mem.set(bytes, ptr);
  const packed = parse(ptr, bytes.length, header ? 1 : 0);
  refreshMem();
  const outPtr = Number(packed >> 32n);
  const outLen = Number(packed & 0xffffffffn);
  const json = decoder.decode(mem.subarray(outPtr, outPtr + outLen));
  return JSON.parse(json);
}
