# Vendored Z80 SingleStepTests subset

This directory holds a small, **committed** subset of the Z80 processor test
vectors from the SingleStepTests project, used by
`crates/genesoxide-test-harness/tests/z80_suite.rs`.

## Source

- Upstream repo: <https://github.com/SingleStepTests/z80>
- Pinned commit: `ebe1875d48f374bcfd4b505d8eb8ee751568b5f7` (`main` HEAD at vendoring time)
- Path in upstream: `v1/<opcode>.json` (uncompressed JSON, one file per opcode)

The JSON format is used verbatim — the harness's `TestCase` / `TestState` structs
deserialize it directly (fields `af_`, `bc_`, `de_`, `hl_`; `ram: [[addr, val]]`;
`ports: [[addr, val, "r"|"w"]]`).

## What is vendored

Only a **representative subset** is committed: 27 curated opcode files, each
**truncated to the first 100 test vectors** (upstream ships ~1000 per opcode), to
keep the committed footprint small (~2.4 MB) while still exercising every opcode
genuinely. The exact list is the `VENDORED` constant in `z80_suite.rs`.

These files MUST be present in a clean checkout: `z80_suite.rs` **panics** (does
not skip) if a vendored file is missing.

## License

Upstream is **MIT-licensed**. The full upstream `LICENSE` header:

```
MIT License

Copyright (c) 2024 SingleStepTests

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```

## Regenerating / fetching the full corpus

Refresh this vendored subset:

```
scripts/fetch-sst-corpus.sh
```

Fetch the **entire** upstream corpus (all opcodes, all vectors) into the
gitignored `tests/z80-tests/v1/v1/` directory used by the opt-in
`#[ignore] full_suite` test:

```
scripts/fetch-sst-corpus.sh --full
cargo test -p genesoxide-test-harness --test z80_suite full_suite -- --ignored --nocapture
```
