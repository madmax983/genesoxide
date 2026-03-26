//! Parser and runner for the m68000-tests binary test vectors.
//!
//! These are MAME-generated, cycle-accurate test cases for every 68000
//! instruction. Binary format documented in the test suite's `decode.py`.
//!
//! Each `.json.bin` file contains ~2500 test cases. Each test case specifies
//! initial CPU state + RAM, and the expected final state after executing
//! one instruction.

use std::collections::HashMap;
use std::path::Path;

use genesoxide_core::cpu::{Bus, Cpu, StatusRegister, execute_instruction};

// ── Binary format constants ─────────────────────────────────────────────

const FILE_MAGIC: u32 = 0x1A3F_5D71;
const TEST_MAGIC: u32 = 0xABC1_2367;
const NAME_MAGIC: u32 = 0x89AB_CDEF;
const STATE_MAGIC: u32 = 0x0123_4567;
const TRANS_MAGIC: u32 = 0x4567_89AB;

// ── Data structures ─────────────────────────────────────────────────────

/// CPU + memory state for one side of a test case.
#[derive(Debug, Clone)]
pub struct TestState {
    /// Data registers D0-D7.
    pub d: [u32; 8],
    /// Address registers A0-A6.
    pub a: [u32; 7],
    /// User stack pointer.
    pub usp: u32,
    /// Supervisor stack pointer.
    pub ssp: u32,
    /// Status register (full 16-bit value stored as u32).
    pub sr: u32,
    /// Program counter ("next prefetch address" = instruction_addr + 4).
    pub pc: u32,
    /// Prefetch queue: [opcode, next_word].
    pub prefetch: [u32; 2],
    /// RAM contents as (word-aligned address, big-endian u16 value) pairs.
    pub ram: Vec<(u32, u16)>,
}

/// A single test case from the m68000-tests suite.
#[derive(Debug, Clone)]
pub struct TestCase {
    /// Human-readable test name (e.g., "MOVEQ #0, D0").
    pub name: String,
    /// CPU + memory state before execution.
    pub initial: TestState,
    /// Expected CPU + memory state after execution.
    pub expected: TestState,
    /// Expected cycle count.
    pub cycles: u32,
}

/// Result of running one test case.
#[derive(Debug)]
pub struct TestFailure {
    pub test_name: String,
    pub mismatches: Vec<Mismatch>,
}

/// A single field mismatch between expected and actual state.
#[derive(Debug)]
pub struct Mismatch {
    pub field: String,
    pub expected: u32,
    pub actual: u32,
}

// ── Binary parser ───────────────────────────────────────────────────────

/// Cursor into the binary data.
struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    fn remaining(&self) -> usize {
        self.data.len().saturating_sub(self.pos)
    }

    fn read_u8(&mut self) -> u8 {
        let val = self.data[self.pos];
        self.pos += 1;
        val
    }

    fn read_u16_le(&mut self) -> u16 {
        let val = u16::from_le_bytes([self.data[self.pos], self.data[self.pos + 1]]);
        self.pos += 2;
        val
    }

    fn read_u32_le(&mut self) -> u32 {
        let val = u32::from_le_bytes([
            self.data[self.pos],
            self.data[self.pos + 1],
            self.data[self.pos + 2],
            self.data[self.pos + 3],
        ]);
        self.pos += 4;
        val
    }

    fn read_bytes(&mut self, n: usize) -> &'a [u8] {
        let slice = &self.data[self.pos..self.pos + n];
        self.pos += n;
        slice
    }

    /// Reads a section header: (numbytes: u32, magic: u32). Returns numbytes.
    fn read_section_header(&mut self, expected_magic: u32) -> u32 {
        let numbytes = self.read_u32_le();
        let magic = self.read_u32_le();
        assert_eq!(
            magic,
            expected_magic,
            "Bad magic: expected {expected_magic:#010X}, got {magic:#010X} at offset {}",
            self.pos - 4
        );
        numbytes
    }
}

/// Parses a test name from the binary stream.
fn parse_name(r: &mut Reader<'_>) -> String {
    r.read_section_header(NAME_MAGIC);
    let strlen = r.read_u32_le() as usize;
    let bytes = r.read_bytes(strlen);
    String::from_utf8_lossy(bytes).into_owned()
}

/// Register order in the binary format (19 registers, each u32 LE).
/// d0-d7, a0-a6, usp, ssp, sr, pc
fn parse_state(r: &mut Reader<'_>) -> TestState {
    r.read_section_header(STATE_MAGIC);

    let mut d = [0u32; 8];
    for reg in &mut d {
        *reg = r.read_u32_le();
    }

    let mut a = [0u32; 7];
    for reg in &mut a {
        *reg = r.read_u32_le();
    }

    let usp = r.read_u32_le();
    let ssp = r.read_u32_le();
    let sr = r.read_u32_le();
    let pc = r.read_u32_le();

    let pf0 = r.read_u32_le();
    let pf1 = r.read_u32_le();

    let num_rams = r.read_u32_le() as usize;
    let mut ram = Vec::with_capacity(num_rams);
    for _ in 0..num_rams {
        let addr = r.read_u32_le();
        let data = r.read_u16_le();
        debug_assert!(
            addr < 0x100_0000,
            "RAM address out of 24-bit range: {addr:#X}"
        );
        ram.push((addr, data));
    }

    TestState {
        d,
        a,
        usp,
        ssp,
        sr,
        pc,
        prefetch: [pf0, pf1],
        ram,
    }
}

/// Skips over the transaction log (we don't need it for state comparison).
fn skip_transactions(r: &mut Reader<'_>) -> u32 {
    r.read_section_header(TRANS_MAGIC);
    let num_cycles = r.read_u32_le();
    let num_transactions = r.read_u32_le();

    for _ in 0..num_transactions {
        let tw = r.read_u8();
        let _cycles = r.read_u32_le();
        if tw != 0 {
            // fc, addr_bus, data_bus, UDS, LDS — 5 × u32
            r.pos += 20;
        }
    }

    num_cycles
}

/// Parses one test case from the binary stream.
fn parse_test(r: &mut Reader<'_>) -> TestCase {
    r.read_section_header(TEST_MAGIC);
    let name = parse_name(r);
    let initial = parse_state(r);
    let expected = parse_state(r);
    let cycles = skip_transactions(r);

    TestCase {
        name,
        initial,
        expected,
        cycles,
    }
}

/// Loads all test cases from a `.json.bin` file.
pub fn load_test_file(path: &Path) -> Vec<TestCase> {
    let data =
        std::fs::read(path).unwrap_or_else(|e| panic!("Failed to read {}: {e}", path.display()));
    let mut r = Reader::new(&data);

    let magic = r.read_u32_le();
    assert_eq!(magic, FILE_MAGIC, "Not a valid m68000-tests binary file");
    let num_tests = r.read_u32_le() as usize;

    let mut tests = Vec::with_capacity(num_tests);
    for _ in 0..num_tests {
        tests.push(parse_test(&mut r));
    }

    assert_eq!(
        r.remaining(),
        0,
        "Trailing data: {} bytes unread",
        r.remaining()
    );

    tests
}

// ── Test bus ────────────────────────────────────────────────────────────

/// A flat 16MB memory space for running isolated CPU tests.
///
/// Uses a HashMap for sparse storage — tests typically only touch
/// a handful of addresses.
pub struct TestBus {
    mem: HashMap<u32, u8>,
}

impl TestBus {
    pub fn new() -> Self {
        Self {
            mem: HashMap::with_capacity(256),
        }
    }

    /// Loads RAM entries from a test state (word-aligned addr, u16 value).
    pub fn load_ram(&mut self, ram: &[(u32, u16)]) {
        for &(addr, val) in ram {
            let addr = addr & 0x00FF_FFFF;
            // Big-endian: high byte at addr, low byte at addr+1
            self.mem.insert(addr, (val >> 8) as u8);
            self.mem.insert(addr | 1, val as u8);
        }
    }

    /// Reads a byte, returning 0 for unmapped addresses.
    fn peek(&self, addr: u32) -> u8 {
        *self.mem.get(&(addr & 0x00FF_FFFF)).unwrap_or(&0)
    }

    /// Writes a byte.
    fn poke(&mut self, addr: u32, val: u8) {
        self.mem.insert(addr & 0x00FF_FFFF, val);
    }
}

impl Bus for TestBus {
    fn read_byte(&mut self, addr: u32) -> u8 {
        self.peek(addr)
    }

    fn read_word(&mut self, addr: u32) -> u16 {
        let hi = self.peek(addr) as u16;
        let lo = self.peek(addr.wrapping_add(1)) as u16;
        (hi << 8) | lo
    }

    fn write_byte(&mut self, addr: u32, val: u8) {
        self.poke(addr, val);
    }

    fn write_word(&mut self, addr: u32, val: u16) {
        self.poke(addr, (val >> 8) as u8);
        self.poke(addr.wrapping_add(1), val as u8);
    }
}

// ── Test runner ─────────────────────────────────────────────────────────

/// Sets up a CPU from a test's initial state.
fn load_cpu_state(cpu: &mut Cpu, state: &TestState) {
    cpu.d = state.d;
    cpu.a = state.a;
    cpu.usp = state.usp;
    cpu.ssp = state.ssp;
    cpu.sr = StatusRegister::new(state.sr as u16);
    // The test's PC is "next prefetch address" = instruction_addr + 4.
    // Our executor expects PC past the opcode word, so: instruction_addr + 2 = pc - 2.
    cpu.pc = state.pc.wrapping_sub(2);
    cpu.cycles = 0;
    cpu.halted = false;
    cpu.stopped = false;
}

/// Compares actual CPU state against expected, returning any mismatches.
fn compare_state(cpu: &Cpu, bus: &TestBus, expected: &TestState) -> Vec<Mismatch> {
    let mut mismatches = Vec::new();

    // Data registers
    for i in 0..8 {
        if cpu.d[i] != expected.d[i] {
            mismatches.push(Mismatch {
                field: format!("D{i}"),
                expected: expected.d[i],
                actual: cpu.d[i],
            });
        }
    }

    // Address registers A0-A6
    for i in 0..7 {
        if cpu.a[i] != expected.a[i] {
            mismatches.push(Mismatch {
                field: format!("A{i}"),
                expected: expected.a[i],
                actual: cpu.a[i],
            });
        }
    }

    // Stack pointers
    if cpu.usp != expected.usp {
        mismatches.push(Mismatch {
            field: "USP".into(),
            expected: expected.usp,
            actual: cpu.usp,
        });
    }
    if cpu.ssp != expected.ssp {
        mismatches.push(Mismatch {
            field: "SSP".into(),
            expected: expected.ssp,
            actual: cpu.ssp,
        });
    }

    // Status register
    let actual_sr = cpu.sr.0 as u32;
    if actual_sr != expected.sr {
        mismatches.push(Mismatch {
            field: "SR".into(),
            expected: expected.sr,
            actual: actual_sr,
        });
    }

    // PC mapping: the test's PC is the "next prefetch address", which is
    // normally 4 bytes ahead of where our emulator's PC ends up (the 68000
    // keeps a 2-word prefetch queue). However, when the CPU is stopped
    // (STOP instruction), no additional prefetch occurs, so the offset is 0.
    let prefetch_offset = if cpu.stopped { 0 } else { 4 };
    let expected_our_pc = expected.pc.wrapping_sub(prefetch_offset);
    if cpu.pc != expected_our_pc {
        mismatches.push(Mismatch {
            field: "PC".into(),
            expected: expected_our_pc,
            actual: cpu.pc,
        });
    }

    // RAM — check every expected word
    for &(addr, expected_val) in &expected.ram {
        let addr = addr & 0x00FF_FFFF;
        let actual_hi = bus.peek(addr) as u16;
        let actual_lo = bus.peek(addr | 1) as u16;
        let actual_val = (actual_hi << 8) | actual_lo;
        if actual_val != expected_val {
            mismatches.push(Mismatch {
                field: format!("RAM[{addr:#08X}]"),
                expected: expected_val as u32,
                actual: actual_val as u32,
            });
        }
    }

    mismatches
}

/// Runs a single test case. Returns `None` on success, `Some(failure)` on mismatch.
pub fn run_test(test: &TestCase) -> Option<TestFailure> {
    let mut cpu = Cpu::new();
    let mut bus = TestBus::new();

    // Load initial state
    load_cpu_state(&mut cpu, &test.initial);
    bus.load_ram(&test.initial.ram);

    // The opcode is prefetch[0]
    let opcode = test.initial.prefetch[0] as u16;

    // Execute one instruction
    let _cycles = execute_instruction(&mut cpu, opcode, &mut bus);

    // Compare against expected
    let mismatches = compare_state(&cpu, &bus, &test.expected);

    if mismatches.is_empty() {
        None
    } else {
        Some(TestFailure {
            test_name: test.name.clone(),
            mismatches,
        })
    }
}

/// Returns true if this test case appears to trigger an exception (address error,
/// privilege violation, etc.) based on the expected state.
///
/// Heuristic: if the expected SSP is lower than the initial SSP (stack grew due to
/// exception frame push), this is an exception-generating test case.
pub fn is_exception_test(test: &TestCase) -> bool {
    test.expected.ssp < test.initial.ssp
}

/// Runs all tests from a file. Returns (passed, failed, Vec<first N failures>).
pub fn run_test_file(path: &Path, max_failures: usize) -> (usize, usize, Vec<TestFailure>) {
    run_test_file_filtered(path, max_failures, false)
}

/// Runs tests from a file, optionally skipping exception-generating tests.
/// Returns (passed, failed, Vec<first N failures>).
pub fn run_test_file_filtered(
    path: &Path,
    max_failures: usize,
    skip_exceptions: bool,
) -> (usize, usize, Vec<TestFailure>) {
    let tests = load_test_file(path);
    let mut passed = 0;
    let mut failed = 0;
    let mut failures = Vec::new();

    for test in &tests {
        if skip_exceptions && is_exception_test(test) {
            continue;
        }
        match run_test(test) {
            None => passed += 1,
            Some(failure) => {
                failed += 1;
                if failures.len() < max_failures {
                    failures.push(failure);
                }
            }
        }
    }

    (passed, failed, failures)
}
