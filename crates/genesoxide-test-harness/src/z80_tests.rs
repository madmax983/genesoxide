//! Parser and runner for the jsmoo Z80 test vectors.
//!
//! These are JSON-based, cycle-accurate test cases for every Z80
//! instruction. Each `.json` file contains ~1000 test cases for a
//! single opcode (or prefix+opcode combination).
//!
//! Each test case specifies initial CPU state + RAM, and the expected
//! final state after executing one instruction.

use std::collections::HashMap;
use std::path::Path;

use genesoxide_core::z80::{Z80, execute::Bus, execute_instruction};
use serde::Deserialize;

// ── Data structures ─────────────────────────────────────────────────────

/// CPU + memory state from the JSON test vector.
#[derive(Debug, Clone, Deserialize)]
pub struct TestState {
    pub pc: u16,
    pub sp: u16,
    pub a: u8,
    pub b: u8,
    pub c: u8,
    pub d: u8,
    pub e: u8,
    pub f: u8,
    pub h: u8,
    pub l: u8,
    pub i: u8,
    pub r: u8,
    /// EI pending state (0 or 1).
    pub ei: u8,
    /// Internal WZ register (tracked but not compared).
    pub wz: u16,
    pub ix: u16,
    pub iy: u16,
    /// Shadow AF as u16 (A' = high byte, F' = low byte).
    #[serde(rename = "af_")]
    pub af_prime: u16,
    /// Shadow BC as u16 (B' = high byte, C' = low byte).
    #[serde(rename = "bc_")]
    pub bc_prime: u16,
    /// Shadow DE as u16 (D' = high byte, E' = low byte).
    #[serde(rename = "de_")]
    pub de_prime: u16,
    /// Shadow HL as u16 (H' = high byte, L' = low byte).
    #[serde(rename = "hl_")]
    pub hl_prime: u16,
    /// Interrupt mode (0, 1, or 2).
    pub im: u8,
    /// Internal P state (ignored).
    #[serde(default)]
    pub p: u8,
    /// Internal Q state (ignored).
    #[serde(default)]
    pub q: u8,
    /// Interrupt flip-flop 1 (0 or 1).
    pub iff1: u8,
    /// Interrupt flip-flop 2 (0 or 1).
    pub iff2: u8,
    /// RAM entries as [address, value] pairs.
    pub ram: Vec<[u16; 2]>,
}

/// A port I/O entry from the jsmoo test vector: [address, value, direction].
/// Direction is "r" for read, "w" for write.
#[derive(Debug, Clone, Deserialize)]
pub struct PortEntry(pub u16, pub u8, pub String);

/// A single test case from the jsmoo Z80 test suite.
#[derive(Debug, Clone, Deserialize)]
pub struct TestCase {
    /// Human-readable test name (e.g., "00 0000").
    pub name: String,
    /// CPU + memory state before execution.
    pub initial: TestState,
    /// Expected CPU + memory state after execution.
    #[serde(rename = "final")]
    pub expected: TestState,
    /// Bus cycle log (not used for state comparison).
    #[serde(default)]
    pub cycles: Vec<serde_json::Value>,
    /// Port I/O entries — reads provide data, writes are verified.
    #[serde(default)]
    pub ports: Vec<PortEntry>,
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
    pub expected: String,
    pub actual: String,
}

// ── Test bus ────────────────────────────────────────────────────────────

/// A flat 64KB memory space with port I/O support for running isolated Z80 tests.
///
/// Port reads return pre-loaded values by 16-bit address.
/// Port writes are recorded for later comparison.
pub struct TestBus {
    mem: [u8; 65536],
    /// Pre-loaded port read values, keyed by 16-bit port address.
    port_reads: HashMap<u16, u8>,
    /// Recorded port writes as (address, value) pairs.
    port_writes: Vec<(u16, u8)>,
}

impl TestBus {
    pub fn new() -> Self {
        Self {
            mem: [0u8; 65536],
            port_reads: HashMap::new(),
            port_writes: Vec::new(),
        }
    }

    /// Loads RAM entries from a test state ([addr, val] pairs).
    pub fn load_ram(&mut self, ram: &[[u16; 2]]) {
        for entry in ram {
            let addr = entry[0] as usize;
            let val = entry[1] as u8;
            self.mem[addr] = val;
        }
    }

    /// Pre-loads port read values from test vector port entries.
    pub fn load_ports(&mut self, ports: &[PortEntry]) {
        for entry in ports {
            if entry.2 == "r" {
                self.port_reads.insert(entry.0, entry.1);
            }
        }
    }

    /// Reads a byte from memory (for comparison).
    pub fn peek(&self, addr: u16) -> u8 {
        self.mem[addr as usize]
    }

    /// Returns recorded port writes for comparison.
    pub fn port_writes(&self) -> &[(u16, u8)] {
        &self.port_writes
    }
}

impl Bus for TestBus {
    fn read_byte(&mut self, addr: u16) -> u8 {
        self.mem[addr as usize]
    }

    fn write_byte(&mut self, addr: u16, val: u8) {
        self.mem[addr as usize] = val;
    }

    fn read_port(&mut self, port: u16) -> u8 {
        self.port_reads.get(&port).copied().unwrap_or(0xFF)
    }

    fn write_port(&mut self, port: u16, val: u8) {
        self.port_writes.push((port, val));
    }
}

// ── State loading ──────────────────────────────────────────────────────

/// Sets up a Z80 CPU from a test's initial state.
pub fn load_cpu_state(cpu: &mut Z80, state: &TestState) {
    cpu.a = state.a;
    cpu.f = state.f;
    cpu.b = state.b;
    cpu.c = state.c;
    cpu.d = state.d;
    cpu.e = state.e;
    cpu.h = state.h;
    cpu.l = state.l;

    // Shadow registers come as u16 pairs — split into individual bytes.
    cpu.a_prime = (state.af_prime >> 8) as u8;
    cpu.f_prime = state.af_prime as u8;
    cpu.b_prime = (state.bc_prime >> 8) as u8;
    cpu.c_prime = state.bc_prime as u8;
    cpu.d_prime = (state.de_prime >> 8) as u8;
    cpu.e_prime = state.de_prime as u8;
    cpu.h_prime = (state.hl_prime >> 8) as u8;
    cpu.l_prime = state.hl_prime as u8;

    cpu.ix = state.ix;
    cpu.iy = state.iy;
    cpu.sp = state.sp;
    cpu.pc = state.pc;
    cpu.i = state.i;
    cpu.r = state.r;

    cpu.iff1 = state.iff1 != 0;
    cpu.iff2 = state.iff2 != 0;
    cpu.im = state.im;
    cpu.ei_pending = state.ei != 0;

    cpu.halted = false;
    cpu.cycles = 0;
    cpu.wz = state.wz;
}

// ── State comparison ───────────────────────────────────────────────────

/// Compares actual Z80 state against expected, returning any mismatches.
pub fn compare_state(
    cpu: &Z80,
    bus: &TestBus,
    expected: &TestState,
    expected_ports: &[PortEntry],
) -> Vec<Mismatch> {
    let mut mismatches = Vec::new();

    macro_rules! check_u8 {
        ($field:expr, $expected:expr, $actual:expr) => {
            if $expected != $actual {
                mismatches.push(Mismatch {
                    field: $field.into(),
                    expected: format!("{:#04X}", $expected),
                    actual: format!("{:#04X}", $actual),
                });
            }
        };
    }

    macro_rules! check_u16 {
        ($field:expr, $expected:expr, $actual:expr) => {
            if $expected != $actual {
                mismatches.push(Mismatch {
                    field: $field.into(),
                    expected: format!("{:#06X}", $expected),
                    actual: format!("{:#06X}", $actual),
                });
            }
        };
    }

    // Main registers
    check_u8!("A", expected.a, cpu.a);
    check_u8!("F", expected.f, cpu.f);
    check_u8!("B", expected.b, cpu.b);
    check_u8!("C", expected.c, cpu.c);
    check_u8!("D", expected.d, cpu.d);
    check_u8!("E", expected.e, cpu.e);
    check_u8!("H", expected.h, cpu.h);
    check_u8!("L", expected.l, cpu.l);

    // Shadow registers — reassemble from individual bytes for comparison.
    let actual_af_prime = (u16::from(cpu.a_prime) << 8) | u16::from(cpu.f_prime);
    let actual_bc_prime = (u16::from(cpu.b_prime) << 8) | u16::from(cpu.c_prime);
    let actual_de_prime = (u16::from(cpu.d_prime) << 8) | u16::from(cpu.e_prime);
    let actual_hl_prime = (u16::from(cpu.h_prime) << 8) | u16::from(cpu.l_prime);

    check_u16!("AF'", expected.af_prime, actual_af_prime);
    check_u16!("BC'", expected.bc_prime, actual_bc_prime);
    check_u16!("DE'", expected.de_prime, actual_de_prime);
    check_u16!("HL'", expected.hl_prime, actual_hl_prime);

    // Index registers
    check_u16!("IX", expected.ix, cpu.ix);
    check_u16!("IY", expected.iy, cpu.iy);

    // Control registers
    check_u16!("SP", expected.sp, cpu.sp);
    check_u16!("PC", expected.pc, cpu.pc);
    check_u8!("I", expected.i, cpu.i);
    check_u8!("R", expected.r, cpu.r);

    // Interrupt state
    let actual_iff1: u8 = if cpu.iff1 { 1 } else { 0 };
    let actual_iff2: u8 = if cpu.iff2 { 1 } else { 0 };
    let actual_ei: u8 = if cpu.ei_pending { 1 } else { 0 };

    if expected.iff1 != actual_iff1 {
        mismatches.push(Mismatch {
            field: "IFF1".into(),
            expected: format!("{}", expected.iff1),
            actual: format!("{}", actual_iff1),
        });
    }
    if expected.iff2 != actual_iff2 {
        mismatches.push(Mismatch {
            field: "IFF2".into(),
            expected: format!("{}", expected.iff2),
            actual: format!("{}", actual_iff2),
        });
    }
    if expected.ei != actual_ei {
        mismatches.push(Mismatch {
            field: "EI".into(),
            expected: format!("{}", expected.ei),
            actual: format!("{}", actual_ei),
        });
    }
    if expected.im != cpu.im {
        mismatches.push(Mismatch {
            field: "IM".into(),
            expected: format!("{}", expected.im),
            actual: format!("{}", cpu.im),
        });
    }

    // RAM — check every expected byte.
    for entry in &expected.ram {
        let addr = entry[0];
        let expected_val = entry[1] as u8;
        let actual_val = bus.peek(addr);
        if expected_val != actual_val {
            mismatches.push(Mismatch {
                field: format!("RAM[{addr:#06X}]"),
                expected: format!("{:#04X}", expected_val),
                actual: format!("{:#04X}", actual_val),
            });
        }
    }

    // Port writes — check expected write operations match.
    let expected_writes: Vec<_> = expected_ports.iter().filter(|e| e.2 == "w").collect();
    let actual_writes = bus.port_writes();

    for (i, exp) in expected_writes.iter().enumerate() {
        if let Some(actual) = actual_writes.get(i) {
            if exp.0 != actual.0 || exp.1 != actual.1 {
                mismatches.push(Mismatch {
                    field: format!("PORT_WRITE[{i}]"),
                    expected: format!("({:#06X}, {:#04X})", exp.0, exp.1),
                    actual: format!("({:#06X}, {:#04X})", actual.0, actual.1),
                });
            }
        } else {
            mismatches.push(Mismatch {
                field: format!("PORT_WRITE[{i}]"),
                expected: format!("({:#06X}, {:#04X})", exp.0, exp.1),
                actual: "MISSING".into(),
            });
        }
    }

    mismatches
}

// ── File loading ───────────────────────────────────────────────────────

/// Loads all test cases from a jsmoo `.json` file.
pub fn load_test_file(path: &Path) -> Vec<TestCase> {
    let data = std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("Failed to read {}: {e}", path.display()));
    serde_json::from_str(&data)
        .unwrap_or_else(|e| panic!("Failed to parse {}: {e}", path.display()))
}

// ── Test runner ─────────────────────────────────────────────────────────

/// Runs a single test case. Returns `None` on success, `Some(failure)` on mismatch.
pub fn run_test(test: &TestCase) -> Option<TestFailure> {
    let mut cpu = Z80::new();
    let mut bus = TestBus::new();

    // Load initial state
    load_cpu_state(&mut cpu, &test.initial);
    bus.load_ram(&test.initial.ram);
    bus.load_ports(&test.ports);

    // Execute one instruction
    let _cycles = execute_instruction(&mut cpu, &mut bus);

    // Compare against expected
    let mismatches = compare_state(&cpu, &bus, &test.expected, &test.ports);

    if mismatches.is_empty() {
        None
    } else {
        Some(TestFailure {
            test_name: test.name.clone(),
            mismatches,
        })
    }
}

/// Runs all tests from a file. Returns (passed, failed, Vec<first N failures>).
pub fn run_test_file(path: &Path, max_failures: usize) -> (usize, usize, Vec<TestFailure>) {
    let tests = load_test_file(path);
    let mut passed = 0;
    let mut failed = 0;
    let mut failures = Vec::new();

    for test in &tests {
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
