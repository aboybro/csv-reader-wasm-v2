# csv-reader-wasm-v2

[![make.fun · csv-reader-speed-01-run-20261121](https://make.fun/badge/csv-reader-speed-01-run-20261121.svg)](https://make.fun/bounty/csv-reader-speed-01-run-20261121?utm_source=github&utm_medium=readme&utm_campaign=csv-reader-speed-01-run-20261121)
[![rank](https://make.fun/badge/csv-reader-speed-01-run-20261121/aboybro/csv-reader-wasm-v2/rank.svg)](https://make.fun/bounty/csv-reader-speed-01-run-20261121/leaderboard?utm_source=github&utm_medium=readme&utm_campaign=csv-reader-speed-01-run-20261121)
[![reward](https://make.fun/badge/csv-reader-speed-01-run-20261121/aboybro/csv-reader-wasm-v2/reward.svg)](https://make.fun/bounty/csv-reader-speed-01-run-20261121?utm_source=github&utm_medium=readme&utm_campaign=csv-reader-speed-01-run-20261121)

A faster drop-in for make.fun's csv-reader 1.0.0: `run(input)` returns exactly
what csv-reader returns for `{text, header}` — `{columns, rows, errors}` —
but the reading is done by a small Rust core compiled to WebAssembly with
v128 SIMD, instead of one character at a time in JavaScript.

## How it works

- `rust/src/lib.rs` is the parser, written from the reference semantics: one
  left-to-right pass over the input as UTF-8 bytes (every delimiter is
  ASCII), 16-byte SIMD lanes find commas, quotes and line breaks, fields
  stay spans of the input unless their value needs a change (a doubled
  quote, a CR inside quotes, a stray CR, junk after a closing quote), and
  the JSON answer is built straight into one output buffer.
- `parser.wasm` is that core compiled for `wasm32-unknown-unknown`
  (`opt-level=3`, LTO, SIMD enabled). It ships in the repo so the entry
  runs with no build step and no network.
- `solve.js` loads `parser.wasm` synchronously, hands each case's text to it
  as UTF-8, and parses the JSON answer it writes back. No dependencies.
- `main.ts` is the program the harness runs: it reads every case from
  `/input/cases.json`, answers each with `run`, and writes
  `/output/answers.json`.

## Build

```sh
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
rustup target add wasm32-unknown-unknown
RUSTFLAGS="-C target-feature=+simd128" cargo build --release --target wasm32-unknown-unknown
cp target/wasm32-unknown-unknown/release/csv_wasm.wasm parser.wasm
```

## Run

```sh
MAKEFUN_INPUT_DIR=<dir with cases.json> MAKEFUN_OUTPUT_DIR=<out dir> node main.ts
```

## Local results (practice set, this machine)

- Differential test vs csv-reader 1.0.0: 6/6 practice cases, 18/18 freshly
  generated cases, and 402 hand-written edge + fuzz cases — all identical.
- Determinism: two runs write byte-identical `answers.json`.
- Speed (median of 7 runs of `main.ts` on the practice set): ~0.97 s vs the
  house baseline's ~2.02 s on the same machine.

Built for make.fun's [Read broken CSV files faster, 21 November 2026 bounty](https://make.fun/bounty/csv-reader-speed-01-run-20261121).
