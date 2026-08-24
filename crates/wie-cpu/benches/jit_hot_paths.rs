//! Criterion regression benches for the wie-cpu hot paths, driven exclusively
//! through the crate's **public** API surface (`CpuEngine` + `JitCpu` /
//! `IcedCpu`).
//!
//! Organization (docs/perf-plan-2.md, G5/G6): every case lives in a group so
//! criterion reports `<group>/<case>` and comparisons line up mechanically:
//!
//! - `exec/*`      — sustained execution throughput in **insns/s**
//!   (`Throughput::Elements`). Headline regression metric: bundles dispatch,
//!   chaining, TLB hits, and generated-code quality into one number per
//!   backend. `jit_*` vs `iced_*` gives the speedup ratio in one report.
//! - `dispatch/*`  — block-boundary cost: a chain of tiny 2-insn blocks where
//!   nearly all time is block entry/exit (+ chaining when `WIE_JIT_CHAIN`
//!   is on). Read against `exec/*` to derive per-transition overhead.
//! - `compile/*`   — inline compile latency by block size (small/medium/
//!   large). Guards background-worker economics: promotion thresholds and
//!   cooldown windows calibrate against these numbers.
//! - `mem/*`       — soft-translate load/store paths (hot span vs copying API
//!   vs cross-page walk).
//!
//! TERMINATION NOTE: a JIT'd `jmp $` self-loop chains *inside* generated
//! code and never returns to the host budget loop (`run_until_stop` only
//! checks `count` between block executions), so naive self-loops hang. Every
//! terminating workload here ends by jumping to an `UNTIL` sentinel VA the
//! host loop checks, or by a conditional back-edge whose count runs out.
//!
//! Regression workflow (scripts/bench-gate.sh):
//! ```text
//!   ./scripts/bench-gate.sh save main          # on a known-good tree
//!   ...make changes...
//!   ./scripts/bench-gate.sh check main 10      # fail if median regressed >10%
//! ```

use std::hint::black_box;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use wie_cpu::{CpuEngine, IcedCpu, JitCpu, RwxPerms};

/// Guest VAs for bench regions (arbitrary; arenas are demand-zero).
const DATA_BASE: u64 = 0x0000_6000_0000;
const CODE_BASE: u64 = 0x0000_6100_0000;
const STEP_BASE: u64 = 0x0000_6200_0000;
/// Sentinel VA programs jump to so `run_until_stop` breaks cleanly. Never
/// mapped, never fetched: the host loop stops when RIP equals it.
const UNTIL: u64 = 0x0000_6F00_0000;

/// Stride between synthetic block slots. Must exceed the largest benched
/// program (~400 B for the 96-insn body + jmp) or consecutive writes
/// overwrite earlier programs mid-region and executed code becomes garbage.
const CODE_STRIDE: u64 = 512;
/// Distinct pre-written slots for compile benches (always-fresh addresses).
const CODE_SLOTS: usize = 32 * 1024;

fn slot_addr(slot: usize) -> u64 {
    CODE_BASE + (slot as u64) * CODE_STRIDE
}

/// Write `bytes` to consecutive stride-separated slots, stopping before the
/// mapping tail (arena regions keep a small unwritable guard gap past their
/// nominal end — measured ~384 B on a 4 MiB region).
fn fill_slots(cpu: &mut JitCpu, base: u64, size: usize, stride: u64, bytes: &[u8], what: &str) {
    let end = base + size as u64;
    let mut at = base;
    while at + bytes.len() as u64 <= end.saturating_sub(4096) {
        cpu.mem_write(at, bytes)
            .unwrap_or_else(|e| panic!("write {what} @ {at:#x}: {e}"));
        at += stride;
    }
}

/// ALU-only instruction stream of exactly `target_insns` instructions
/// (no terminator).
fn alu_body(target_insns: usize) -> Vec<u8> {
    let rr = |b: &mut Vec<u8>, op: u8, modrm: u8| b.extend_from_slice(&[0x48, op, modrm]);
    let ri = |b: &mut Vec<u8>, modrm: u8, imm: u8| b.extend_from_slice(&[0x48, 0x83, modrm, imm]);
    let mut b = vec![0x48, 0x89, 0xD8]; // mov rax, rbx  (insn 1)
    let mut n = 1_usize;
    while n + 1 < target_insns {
        match b.len() % 4 {
            0 => ri(&mut b, 0xC0, 1),    // add rax, 1   (4B)
            1 => ri(&mut b, 0xE9, 2),    // sub rcx, 2   (4B)
            2 => rr(&mut b, 0x01, 0xD8), // add rax, rbx (3B)
            _ => rr(&mut b, 0x29, 0xD1), // sub rcx, rdx (3B)
        }
        n += 1;
    }
    b
}

/// Body + `jmp rel32 UNTIL`: a standalone program that retires exactly
/// `insns + 1` instructions from its start VA.
fn line_program(insns: usize) -> Vec<u8> {
    let mut b = alu_body(insns);
    // E9 rel32: rel counts from the end of this jmp.
    let end_of_jmp = b.len() as u64 + 5;
    let off = (UNTIL as i64 - end_of_jmp as i64) as i32;
    b.extend_from_slice(&[0xE9]);
    b.extend_from_slice(&off.to_le_bytes());
    b
}

// ---------------------------------------------------------------- exec ----

fn bench_exec(c: &mut Criterion) {
    let mut g = c.benchmark_group("exec");

    // --- straight-line mixed blocks: execution + inter-block dispatch ------
    // NOTE: the canonical tight-loop metric remains the `long_loop` micro-exe
    // timed by scripts/run-micro-suite.sh; a hand-assembled counted loop here
    // would duplicate it against the decoder's basic-block model. Straight
    // lines keep this bench decoder-model-agnostic.
    const LINE_CALLS_16: usize = 400;
    let prog16 = line_program(16);
    let retired16 = 16_u64;
    let mut cpu = JitCpu::open_x86_64();
    cpu.mem_map(CODE_BASE, CODE_SLOTS * CODE_STRIDE as usize, RwxPerms::ALL)
        .expect("map jit code region");
    for slot in 0..CODE_SLOTS {
        cpu.mem_write(slot_addr(slot), &prog16).expect("write slot");
    }
    for slot in 0..64 {
        cpu.precompile_at(slot_addr(slot));
    }
    let s0 = cpu.cpu_stats().expect("stats").jit_insns;
    for slot in 0..64 {
        cpu.run_until_stop(slot_addr(slot), UNTIL, u64::MAX, usize::MAX, 0, 0)
            .expect("line warmup");
    }
    let s1 = cpu.cpu_stats().expect("stats").jit_insns;
    assert!(
        s1.saturating_sub(s0) >= 64 * retired16,
        "JIT did not retire the expected straight-line insns (jit={s1}/{s0})"
    );

    g.throughput(Throughput::Elements(retired16 * LINE_CALLS_16 as u64));
    g.bench_function("jit_line_16insn", |b| {
        b.iter(|| {
            for i in 0..LINE_CALLS_16 {
                let hook = cpu
                    .run_until_stop(
                        black_box(slot_addr(i % CODE_SLOTS)),
                        black_box(UNTIL),
                        black_box(u64::MAX),
                        black_box(usize::MAX),
                        black_box(0),
                        black_box(0),
                    )
                    .expect("run_until_stop");
                black_box(&hook);
            }
        })
    });

    // Larger bodies: amortizes transitions further, closer to steady compute.
    // Decoder caps blocks at 96 insns *including* the terminator jmp, so the
    // body must stay ≤ 95 or the block silently falls back to iced.
    let prog96 = line_program(90);
    let retired96 = 90_u64;
    const LINE_CALLS_96: usize = 80;
    let base96 = STEP_BASE;
    // Leave the tail of the region unwritten: the arena keeps a guard gap
    // past its nominal end; fill_slots keeps a page of margin.
    let mut lcpu = JitCpu::open_x86_64();
    lcpu.mem_map(STEP_BASE, CODE_SLOTS * CODE_STRIDE as usize, RwxPerms::ALL)
        .expect("map line96 region");
    fill_slots(
        &mut lcpu,
        STEP_BASE,
        CODE_SLOTS * CODE_STRIDE as usize,
        CODE_STRIDE,
        &prog96,
        "96-insn slot",
    );
    for slot in 0..64 {
        lcpu.precompile_at(STEP_BASE + slot as u64 * CODE_STRIDE);
    }
    let w0 = lcpu.cpu_stats().expect("stats").jit_insns;
    for slot in 0..64 {
        lcpu.run_until_stop(
            STEP_BASE + slot as u64 * CODE_STRIDE,
            UNTIL,
            u64::MAX,
            usize::MAX,
            0,
            0,
        )
        .expect("line96 warmup");
    }
    let w1 = lcpu.cpu_stats().expect("stats").jit_insns;
    assert!(
        w1.saturating_sub(w0) >= 64 * retired96,
        "JIT did not retire the expected 96-insn line insns"
    );

    g.throughput(Throughput::Elements(retired96 * LINE_CALLS_96 as u64));
    g.bench_function("jit_line_96insn", |b| {
        b.iter(|| {
            for i in 0..LINE_CALLS_96 {
                let hook = lcpu
                    .run_until_stop(
                        black_box(base96 + (i as u64 % 64) * CODE_STRIDE),
                        black_box(UNTIL),
                        black_box(u64::MAX),
                        black_box(usize::MAX),
                        black_box(0),
                        black_box(0),
                    )
                    .expect("run_until_stop");
                black_box(&hook);
            }
        })
    });

    // --- interpreter denominator ------------------------------------------
    let mut iced = IcedCpu::open_x86_64();
    iced.mem_map(STEP_BASE, CODE_SLOTS * CODE_STRIDE as usize, RwxPerms::ALL)
        .expect("map iced region");
    // Fill every slot: an unwritten slot decodes as `add [rax], al`, whose
    // invalid access returns as an Ok-hook and would fake a fast result.
    for slot in 0..64 {
        iced.mem_write(STEP_BASE + slot as u64 * CODE_STRIDE, &prog16)
            .expect("write iced slot");
    }
    g.throughput(Throughput::Elements(retired16 * LINE_CALLS_16 as u64));
    g.bench_function("iced_line_16insn", |b| {
        b.iter(|| {
            for i in 0..LINE_CALLS_16 {
                let hook = iced
                    .run_until_stop(
                        black_box(STEP_BASE + (i as u64 % 64) * CODE_STRIDE),
                        black_box(UNTIL),
                        black_box(u64::MAX),
                        black_box(usize::MAX),
                        black_box(0),
                        black_box(0),
                    )
                    .expect("run_until_stop");
                black_box(&hook);
            }
        })
    });

    g.finish();
}

// ------------------------------------------------------------- dispatch ----
// Block-transition overhead is derived DIFFERENTIALLY rather than measured
// directly: compare exec/jit_line_16insn (a transition every ~16 insns)
// against exec/jit_line_96insn (one every ~90 insns) — the per-transition
// cost is the slope between them. Hand-assembled multi-block chains are
// deliberately avoided: they exercise decoder edge cases (rel8 reach,
// cross-block chaining conventions) that belong to fuzz/matrix coverage,
// not to a regression gate that must be boring and deterministic.

// -------------------------------------------------------------- compile ----

fn bench_compile_sizes(c: &mut Criterion) {
    let mut cpu = JitCpu::open_x86_64();
    cpu.mem_map(CODE_BASE, CODE_SLOTS * CODE_STRIDE as usize, RwxPerms::ALL)
        .expect("map code region");

    let mut g = c.benchmark_group("compile");
    for (name, insns) in [
        ("block_small", 6_usize),
        ("block_medium", 16),
        ("block_large", 94),
    ] {
        let mut prog = alu_body(insns);
        let end_of_jmp = prog.len() as u64 + 5;
        let off = (UNTIL as i64 - end_of_jmp as i64) as i32;
        prog.extend_from_slice(&[0xE9]);
        prog.extend_from_slice(&off.to_le_bytes());
        fill_slots(
            &mut cpu,
            CODE_BASE,
            CODE_SLOTS * CODE_STRIDE as usize,
            CODE_STRIDE,
            &prog,
            name,
        );
        let c0 = cpu.cpu_stats().expect("stats").compiles;
        cpu.precompile_at(slot_addr(0));
        let c1 = cpu.cpu_stats().expect("stats").compiles;
        assert!(c1 > c0, "{name}: precompile_at did not compile the block");

        g.bench_function(name, |b| {
            let mut next = 1_usize;
            b.iter(|| {
                cpu.precompile_at(black_box(slot_addr(next % CODE_SLOTS)));
                next += 1;
            });
        });
    }
    g.finish();
}

// ------------------------------------------------------------------ mem ----

fn bench_mem_translate(c: &mut Criterion) {
    let mut cpu = JitCpu::open_x86_64();
    cpu.mem_map(DATA_BASE, 2 * 1024 * 1024, RwxPerms::READ_WRITE)
        .expect("map data region");

    let mut g = c.benchmark_group("mem");
    g.throughput(Throughput::Bytes(16));

    g.bench_function("translate_load_store_qword", |b| {
        b.iter(|| {
            let mut back = [0_u8; 8];
            let src = cpu.host_slice(DATA_BASE, 8).expect("read span");
            back.copy_from_slice(src);
            let val = black_box(back);
            let dst = cpu.host_slice_mut(DATA_BASE, 8).expect("write span");
            dst.copy_from_slice(&val);
        });
    });

    g.bench_function("api_read_write_qword", |b| {
        b.iter(|| {
            let mut back = [0_u8; 8];
            cpu.mem_read(DATA_BASE, &mut back).expect("mem_read");
            let val = black_box(back);
            cpu.mem_write(DATA_BASE, &val).expect("mem_write");
        });
    });

    let boundary = DATA_BASE + 4096 - 4;
    g.throughput(Throughput::Bytes(8));
    g.bench_function("api_read_cross_page_qword", |b| {
        b.iter(|| {
            let mut back = [0_u8; 8];
            cpu.mem_read(boundary, &mut back)
                .expect("cross-page mem_read");
            black_box(&back);
        });
    });

    g.finish();
}

criterion_group!(
    benches,
    bench_exec,
    bench_compile_sizes,
    bench_mem_translate
);
criterion_main!(benches);
