//! Converter: SingleStepTests 680x0 JSON -> genesoxide `.json.bin` binary vectors.
//!
//! Usage:
//!   cargo run -p genesoxide-test-harness --example sst_json2bin -- <in.json[.gz]> <out.json.bin> [max_vectors]
//!
//! Reads the upstream SingleStepTests 680x0 corpus (github.com/SingleStepTests/680x0,
//! `68000/v1/<OP>.json.gz`, gzipped JSON), and emits the byte-exact binary format
//! consumed by `src/m68k_tests.rs::load_test_file`.
//!
//! This is OUR OWN tooling (freely licensed as part of genesoxide). The upstream
//! JSON corpus it consumes carries NO explicit license file (see
//! `tests/m68000-tests/README.md` for the attribution / license-uncertainty note).
//!
//! Binary format (all little-endian, no padding):
//!   file:  FILE_MAGIC(u32) num_tests(u32) then num_tests * TEST
//!   TEST:  header[numbytes(u32) TEST_MAGIC(u32)] NAME STATE(initial) STATE(final) TRANS
//!   NAME:  header[numbytes(u32) NAME_MAGIC(u32)] strlen(u32) name_bytes(utf8, len-prefixed)
//!   STATE: header[numbytes(u32) STATE_MAGIC(u32)]
//!            d0..d7(8*u32) a0..a6(7*u32) usp ssp sr(u16 zero-ext as u32) pc
//!            prefetch0 prefetch1 num_rams(u32) then per ram: addr(u32) data(u16)
//!   TRANS: header[numbytes(u32) TRANS_MAGIC(u32)] num_cycles(u32) num_transactions(u32=0)
//!
//! Each section's `numbytes` header field is the TOTAL block length INCLUDING the
//! 8-byte header (so a STATE block is `96 + num_rams*6` bytes, matching the parser
//! documentation). The parser does not validate `numbytes`, but we emit it faithfully.

use std::io::Read;
use std::path::Path;

use flate2::read::GzDecoder;
use serde::Deserialize;

// ── Binary format constants (must match src/m68k_tests.rs) ───────────────
const FILE_MAGIC: u32 = 0x1A3F_5D71;
const TEST_MAGIC: u32 = 0xABC1_2367;
const NAME_MAGIC: u32 = 0x89AB_CDEF;
const STATE_MAGIC: u32 = 0x0123_4567;
const TRANS_MAGIC: u32 = 0x4567_89AB;

// ── Upstream JSON structs (our own deserialization schema) ───────────────

#[derive(Deserialize)]
struct UpstreamState {
    d0: u32,
    d1: u32,
    d2: u32,
    d3: u32,
    d4: u32,
    d5: u32,
    d6: u32,
    d7: u32,
    a0: u32,
    a1: u32,
    a2: u32,
    a3: u32,
    a4: u32,
    a5: u32,
    a6: u32,
    usp: u32,
    ssp: u32,
    sr: u32,
    pc: u32,
    prefetch: [u32; 2],
    /// RAM as BYTE pairs: [byte_addr, byte_value].
    ram: Vec<[i64; 2]>,
}

#[derive(Deserialize)]
struct UpstreamTest {
    name: String,
    initial: UpstreamState,
    #[serde(rename = "final")]
    final_state: UpstreamState,
    /// Upstream cycle count — asserted by the harness (except MUL/DIV).
    length: u32,
    // `transactions` is intentionally ignored; we emit 0 transactions.
}

// ── Little-endian writer helpers ─────────────────────────────────────────

fn push_u16(buf: &mut Vec<u8>, v: u16) {
    buf.extend_from_slice(&v.to_le_bytes());
}

fn push_u32(buf: &mut Vec<u8>, v: u32) {
    buf.extend_from_slice(&v.to_le_bytes());
}

/// Wraps `body` in a section: header[numbytes(total incl. header), magic] + body.
fn section(magic: u32, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(body.len() + 8);
    let numbytes = (body.len() + 8) as u32;
    push_u32(&mut out, numbytes);
    push_u32(&mut out, magic);
    out.extend_from_slice(body);
    out
}

/// Packs upstream byte-pair RAM into 16-bit big-endian words at even addresses,
/// optionally injecting the two prefetch words into the instruction stream.
///
/// Upstream `ram` is `[byte_addr, byte_value]`. The harness stores/compares WORDS
/// at EVEN addresses (hi = data>>8 at addr, lo at addr|1). So for each even
/// address that has any byte present, emit `(e, (byte[e]<<8) | byte[e+1])`, with
/// missing bytes treated as 0. Sorted by address for determinism.
///
/// PREFETCH INJECTION (`inject`): in the upstream SingleStepTests 680x0 model the
/// opcode word lives at `pc` and the first extension word at `pc+2`, but these two
/// words are supplied ONLY in the `prefetch` field — upstream `ram` begins at
/// `pc+4` (any further extension words). Our harness passes `prefetch[0]` as the
/// opcode directly, but reads *every* extension word from the bus starting at
/// `cpu.pc` (= bin.pc - 2 = upstream.pc + 2). So we must materialize the opcode at
/// `pc` and the first extension word at `pc+2` in memory. We inject those two
/// words (big-endian bytes) first, then let upstream ram bytes override, so any
/// self-modified/overlapping bytes still take the upstream value.
fn pack_ram_with_prefetch(ram: &[[i64; 2]], inject: Option<(u32, [u32; 2])>) -> Vec<(u32, u16)> {
    use std::collections::BTreeMap;
    let mut bytes: BTreeMap<u32, u8> = BTreeMap::new();

    if let Some((pc, prefetch)) = inject {
        let op = prefetch[0] as u16;
        let ext1 = prefetch[1] as u16;
        bytes.insert(pc, (op >> 8) as u8);
        bytes.insert(pc.wrapping_add(1), op as u8);
        bytes.insert(pc.wrapping_add(2), (ext1 >> 8) as u8);
        bytes.insert(pc.wrapping_add(3), ext1 as u8);
    }

    // Upstream bytes override any injected prefetch bytes.
    for pair in ram {
        let addr = pair[0] as u32;
        let val = (pair[1] as u32 & 0xFF) as u8;
        bytes.insert(addr, val);
    }

    // Collect the set of even base addresses touched.
    let mut evens: Vec<u32> = bytes.keys().map(|&a| a & !1).collect();
    evens.sort_unstable();
    evens.dedup();

    evens
        .into_iter()
        .map(|e| {
            let hi = *bytes.get(&e).unwrap_or(&0) as u16;
            let lo = *bytes.get(&(e | 1)).unwrap_or(&0) as u16;
            (e, (hi << 8) | lo)
        })
        .collect()
}

/// Serializes one STATE block body (without the section header).
///
/// `inject_prefetch` is true only for the INITIAL state — the running emulator
/// needs the opcode + first extension word present in memory. The FINAL state is
/// packed as the exact upstream expectation (its `ram` never references the
/// pc/pc+2 code words, so they are not compared).
fn state_body(s: &UpstreamState, inject_prefetch: bool) -> Vec<u8> {
    let mut b = Vec::new();
    for r in [s.d0, s.d1, s.d2, s.d3, s.d4, s.d5, s.d6, s.d7] {
        push_u32(&mut b, r);
    }
    for r in [s.a0, s.a1, s.a2, s.a3, s.a4, s.a5, s.a6] {
        push_u32(&mut b, r);
    }
    push_u32(&mut b, s.usp);
    push_u32(&mut b, s.ssp);
    push_u32(&mut b, s.sr & 0xFFFF); // 16-bit zero-extended
    // Upstream `pc` is the opcode address; the harness treats bin.pc as the
    // "next prefetch address" (instruction_addr + 4), so add 4 for both states.
    push_u32(&mut b, s.pc.wrapping_add(4));
    push_u32(&mut b, s.prefetch[0]);
    push_u32(&mut b, s.prefetch[1]);

    let inject = if inject_prefetch {
        Some((s.pc, s.prefetch))
    } else {
        None
    };
    let ram = pack_ram_with_prefetch(&s.ram, inject);
    push_u32(&mut b, ram.len() as u32);
    for (addr, data) in ram {
        push_u32(&mut b, addr);
        push_u16(&mut b, data);
    }
    b
}

/// Serializes one full TEST block (with header).
fn test_block(t: &UpstreamTest) -> Vec<u8> {
    // NAME block.
    let mut name_body = Vec::new();
    let name_bytes = t.name.as_bytes();
    push_u32(&mut name_body, name_bytes.len() as u32);
    name_body.extend_from_slice(name_bytes);
    let name = section(NAME_MAGIC, &name_body);

    // STATE blocks. Only the initial state injects the prefetch code words.
    let initial = section(STATE_MAGIC, &state_body(&t.initial, true));
    let final_ = section(STATE_MAGIC, &state_body(&t.final_state, false));

    // TRANS block: num_cycles = upstream length, num_transactions = 0.
    let mut trans_body = Vec::new();
    push_u32(&mut trans_body, t.length);
    push_u32(&mut trans_body, 0); // no transactions emitted
    let trans = section(TRANS_MAGIC, &trans_body);

    let mut body = Vec::new();
    body.extend_from_slice(&name);
    body.extend_from_slice(&initial);
    body.extend_from_slice(&final_);
    body.extend_from_slice(&trans);
    section(TEST_MAGIC, &body)
}

fn read_input(path: &Path) -> String {
    let raw = std::fs::read(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    if path.extension().and_then(|e| e.to_str()) == Some("gz") {
        let mut s = String::new();
        GzDecoder::new(&raw[..])
            .read_to_string(&mut s)
            .unwrap_or_else(|e| panic!("gunzip {}: {e}", path.display()));
        s
    } else {
        String::from_utf8(raw).expect("input is not valid UTF-8")
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!(
            "usage: {} <in.json[.gz]> <out.json.bin> [max_vectors]",
            args[0]
        );
        std::process::exit(2);
    }
    let in_path = Path::new(&args[1]);
    let out_path = Path::new(&args[2]);
    let max_vectors: Option<usize> = args
        .get(3)
        .map(|s| s.parse().expect("max_vectors must be a number"));

    let json = read_input(in_path);
    let mut tests: Vec<UpstreamTest> =
        serde_json::from_str(&json).unwrap_or_else(|e| panic!("parse {}: {e}", in_path.display()));

    if let Some(n) = max_vectors {
        tests.truncate(n);
    }

    let mut out = Vec::new();
    push_u32(&mut out, FILE_MAGIC);
    push_u32(&mut out, tests.len() as u32);
    for t in &tests {
        out.extend_from_slice(&test_block(t));
    }

    std::fs::write(out_path, &out).unwrap_or_else(|e| panic!("write {}: {e}", out_path.display()));
    eprintln!(
        "wrote {} ({} vectors, {} bytes)",
        out_path.display(),
        tests.len(),
        out.len()
    );
}
