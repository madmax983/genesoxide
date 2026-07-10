# Vendored m68000 (68000) SingleStepTests subset

This directory holds a small, **committed** subset of Motorola 68000 processor
test vectors, converted to the harness's binary `.json.bin` format and consumed by
`crates/genesoxide-test-harness/tests/m68k_suite.rs`.

## ⚠️ License status: UNCONFIRMED

**These vectors are derived from the public SingleStepTests 680x0 corpus
(<https://github.com/SingleStepTests/680x0>), which — unlike its sibling
SingleStepTests repositories — carries NO explicit `LICENSE` file.** At vendoring
time the upstream repo's `LICENSE` path returned HTTP 404.

The sibling corpora (e.g. SingleStepTests/z80) are MIT-licensed, so the 680x0
vectors are *plausibly* intended to be MIT too, but this is **not confirmed**. We
vendor only a tiny derived subset, with attribution, on a good-faith basis. **A
maintainer should confirm the upstream license before any wider distribution or
redistribution of these vectors.** If the license cannot be confirmed, this
directory may need to be removed and the tests switched back to a
download-at-test-time model.

Attribution: test vectors © the SingleStepTests project; see the upstream repo.

## Source

- Upstream repo: <https://github.com/SingleStepTests/680x0>
- Pinned commit: `e0d5ece9670205cc84a0101081837deb446f86a3` (`main` HEAD at vendoring time)
- Path in upstream: `68000/v1/<OP>.json.gz` (gzipped JSON, one file per opcode)

## What is vendored / how it was generated

The upstream gzipped JSON is converted to the packed little-endian `.json.bin`
format by our own tool, `crates/genesoxide-test-harness/examples/sst_json2bin.rs`
(freely licensed as part of genesoxide). Only a **representative subset** is
committed: 33 curated opcodes, each **truncated to the first 150 vectors**
(upstream ships thousands per opcode), for a ~1.8 MB footprint. The exact list is
the `VENDORED` constant in `m68k_suite.rs`.

The converter injects the two prefetch words (opcode + first extension word) into
the initial RAM image, because upstream supplies them only in the `prefetch`
field while the harness reads every extension word from the bus. See the module
doc comment in `sst_json2bin.rs` for the byte-exact format and the prefetch model.

These files MUST be present in a clean checkout: `m68k_suite.rs` **panics** (does
not skip) if a vendored file is missing.

### Previously-excluded opcodes (now fixed & covered)

Four opcodes were previously excluded because the 68000 core disagreed with the
upstream vectors. Those core bugs are now **fixed**, and all four are vendored and
covered here at 100%:

- **BTST** — `BTST Dn,#imm` timing corrected to 10 cycles (was 8)
- **BSET** — register-destination timing is bit-dependent: 6/8 (dynamic) and
  10/12 (static), 2 clocks faster when the target bit is in the low word
- **LINK** — the `LINK A7` quirk now pushes the *decremented* SP (SP-4), not the
  old SP
- **DIVU** — overflow now **preserves** the incoming N/Z flags (the real MC68000
  leaves N/Z unchanged on a DIVU overflow)

Address-error / privilege exception vectors are skipped at run time
(our core does not implement the group-0 exception stack frames these vectors
assert — see `is_exception_test` and the `no_exception_suite` runner).

## Regenerating / fetching the full corpus

Refresh this vendored subset:

```
scripts/fetch-sst-corpus.sh
```

Fetch the **entire** upstream corpus (all opcodes, all vectors), converted to
`.json.bin`, into the gitignored `tests/m68000-tests-full/v1/` directory used by
the opt-in `#[ignore] full_suite` / `no_exception_suite` tests:

```
scripts/fetch-sst-corpus.sh --full
cargo test -p genesoxide-test-harness --test m68k_suite full_suite -- --ignored --nocapture
```
