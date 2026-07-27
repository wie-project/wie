/*
 * Micro-PE: multi-threaded contention + guest-memory throughput benchmark.
 *
 * Purpose is *optimisation*, not just pass/fail. Each phase isolates one
 * mechanism so its cost can be attributed, and prints wall time so runs can be
 * compared across changes. Every phase also self-checks, so a regression that
 * breaks semantics fails loudly instead of just getting faster.
 *
 * What each phase stresses inside WIE (see docs/mt-threads.md, phase4-*.md):
 *
 *   private     Per-thread private pages, zero sharing. The scaling baseline:
 *               each guest thread has its own CPU engine, TLB, sticky pages and
 *               region pins, so this should scale with cores. If it does not,
 *               the cost is in something process-wide, not in memory itself.
 *
 *   page_excl   Threads write a shared buffer but each owns a distinct 4 KiB
 *               guest page. Exercises per-thread sticky/TLB fills over one
 *               arena without any two threads touching the same page.
 *
 *   page_share  Threads write distinct 8-byte slots inside ONE guest page.
 *               Same-page traffic from every thread at once — the sticky-page
 *               and multi-way TLB thrash case, plus host cache-line sharing.
 *
 *   interlock   InterlockedIncrement/ExchangeAdd on one counter. Each is a host
 *               stop into the WinAPI layer, which takes the host_span + host
 *               atomic path (kernel32/sync.rs). Heaviest test of the
 *               process-wide WinAPI mutex.
 *
 *   critsec     EnterCriticalSection/LeaveCriticalSection around a plain
 *               counter. Uncontended enter/leave run as in-guest stubs; under
 *               contention the loser parks, which must drop the shared WinAPI
 *               mutex or every other thread stalls behind it.
 *
 *   readers     All threads read one large shared buffer. Guest memory sits
 *               behind an RwLock, so concurrent readers should not serialise.
 *
 *   va_churn    Threads VirtualAlloc/VirtualFree repeatedly. Every structural
 *               change bumps GuestMemory::generation, which invalidates *every*
 *               thread's TLB, sticky pages and region pins. Cheap to write,
 *               expensive to survive — the phase most likely to expose a
 *               scaling cliff, and not covered by mt_stress.
 *
 *   pingpong    Event handshake between thread pairs. Measures park/wake
 *               latency rather than throughput.
 *
 * Usage (defaults are micro-suite safe):
 *   mt_contention.exe [threads] [iters] [phase_mask_hex]
 *
 *   threads     1..MAX_THREADS          (default 4)
 *   iters       per-thread iterations   (default 64)
 *   phase_mask  bitmask, hex, no 0x     (default ff = all)
 *
 * Benchmark example (raise the API-stop cap — interlock/va_churn host-stop a
 * lot; each Interlocked* is one stop):
 *   wie-cli run --max-api 2000000 micro-exes/out/mt_contention.exe 8 20000 ff
 *
 * Attribute a single phase, e.g. only the WinAPI-mutex path:
 *   wie-cli run --max-api 2000000 micro-exes/out/mt_contention.exe 8 50000 08
 *
 * Exit codes:
 *   0  ok
 *   1  CreateThread failed
 *   2  wait failed
 *   3  VirtualAlloc failed
 *   4  GetProcAddress (Interlocked*) failed
 *   5  event creation failed
 *   10 private checksum mismatch
 *   11 page_excl checksum mismatch
 *   12 page_share slot mismatch
 *   13 interlock counter mismatch
 *   14 critsec counter mismatch
 *   15 readers sum mismatch
 *   16 va_churn failure
 *   17 pingpong handshake mismatch
 *   20 bad argument
 */

#include <windows.h>

#define MAX_THREADS 16

/* Phase bits. */
#define PH_PRIVATE   0x01u
#define PH_PAGE_EXCL 0x02u
#define PH_PAGE_SHR  0x04u
#define PH_INTERLOCK 0x08u
#define PH_CRITSEC   0x10u
#define PH_READERS   0x20u
#define PH_VA_CHURN  0x40u
#define PH_PINGPONG  0x80u

#define GUEST_PAGE  4096u
/* Per-thread private region. Larger than the sticky-page window so the TLB and
 * pins are exercised rather than a single hot page being cached. */
#define PRIV_BYTES  (64u * 1024u)
/* Shared read-only buffer for the readers phase. */
#define SHARED_READ_BYTES (256u * 1024u)
/* VirtualAlloc churn size — small, so the cost is the generation bump and the
 * resulting TLB/pin invalidation rather than the mapping itself. */
#define CHURN_BYTES 4096u
/* va_churn and pingpong are far more expensive per iteration than the memory
 * phases, so they run a fraction of `iters`. */
#define CHURN_DIVISOR 8u
#define PING_DIVISOR  4u

typedef LONG(WINAPI *PFN_InterlockedIncrement)(LONG volatile *);
typedef LONG(WINAPI *PFN_InterlockedExchangeAdd)(LONG volatile *, LONG);

/* ---- configuration (parsed from argv, then read-only) ---- */
static unsigned g_threads = 4;
static unsigned g_iters = 64;
static unsigned g_phases = 0xffu;

/* ---- shared state ---- */
static CRITICAL_SECTION g_cs;
static volatile LONG g_interlocked_counter = 0;
static volatile LONG g_critsec_counter = 0;
static PFN_InterlockedIncrement g_inc;
static PFN_InterlockedExchangeAdd g_xadd;

static volatile unsigned char *g_priv[MAX_THREADS]; /* one region per thread */
static volatile unsigned char *g_page_excl;   /* one 4 KiB page per thread     */
static volatile unsigned char *g_page_shared; /* one page, all threads         */
static volatile unsigned char *g_shared_read; /* read-only fan-in buffer       */

static volatile LONG g_priv_sum[MAX_THREADS];
static volatile LONG g_excl_sum[MAX_THREADS];
static volatile LONG g_read_sum[MAX_THREADS];
static volatile LONG g_churn_fail[MAX_THREADS];
static volatile LONG g_ping_count[MAX_THREADS];

/* Start gate so all threads enter a phase together — without this, early
 * threads finish before later ones start and nothing actually contends. */
static HANDLE g_start;
/* Single-use barrier used before `pingpong`. The earlier phases finish at very
 * different times, and the primary services pings in thread order; without
 * regrouping first, a thread that arrives early can exhaust its wait timeout
 * before the primary reaches its slot. */
static HANDLE g_barrier_gate;
static volatile LONG g_barrier_count = 0;
/* Per-pair events for the pingpong phase. */
static HANDLE g_ping[MAX_THREADS];
static HANDLE g_pong[MAX_THREADS];

static volatile LONG g_fail_code = 0;

/* ---- tiny no-CRT helpers ---- */

static unsigned str_len(const char *s) {
  unsigned n = 0;
  while (s[n] != '\0') {
    n++;
  }
  return n;
}

/* Append decimal `v` to `buf` at `pos`; returns the new position. */
static unsigned put_u32(char *buf, unsigned pos, unsigned v) {
  char tmp[12];
  unsigned n = 0;
  if (v == 0) {
    buf[pos++] = '0';
    return pos;
  }
  while (v > 0 && n < sizeof(tmp)) {
    tmp[n++] = (char)('0' + (v % 10u));
    v /= 10u;
  }
  while (n > 0) {
    buf[pos++] = tmp[--n];
  }
  return pos;
}

static unsigned put_str(char *buf, unsigned pos, const char *s) {
  unsigned i = 0;
  while (s[i] != '\0') {
    buf[pos++] = s[i++];
  }
  return pos;
}

static void emit(const char *text) {
  HANDLE out = GetStdHandle(STD_OUTPUT_HANDLE);
  DWORD written = 0;
  if (out == NULL || out == INVALID_HANDLE_VALUE) {
    return;
  }
  WriteFile(out, text, str_len(text), &written, NULL);
}

/* Echo the run configuration so a benchmark line identifies itself.
 *
 * Deliberately no self-timing: WIE returns fixed constants from GetTickCount
 * and QueryPerformanceCounter so traces stay deterministic, which means the
 * guest cannot measure its own wall time. Time this from the host instead:
 *
 *   /usr/bin/time -p ./target/release/wie-cli run --max-api N ... mt_contention.exe 8 20000 ff
 *   WIE_RUNTIME_PROFILE=1 ...   (per-phase host/emu split + JIT counters)
 *
 * `kbytes` is the guest bytes touched by the memory phases, so host wall time
 * converts directly into guest memory throughput. */
static void report_config(unsigned kbytes) {
  char line[160];
  unsigned p = 0;
  p = put_str(line, p, "mt_contention threads=");
  p = put_u32(line, p, g_threads);
  p = put_str(line, p, " iters=");
  p = put_u32(line, p, g_iters);
  p = put_str(line, p, " phases=");
  p = put_u32(line, p, g_phases);
  p = put_str(line, p, " kbytes=");
  p = put_u32(line, p, kbytes);
  p = put_str(line, p, "\n");
  line[p] = '\0';
  emit(line);
}

/* Decimal / hex parse of one whitespace-delimited token. Returns 0 on success. */
static int parse_u32(const char **pp, unsigned base, unsigned *out) {
  const char *p = *pp;
  unsigned v = 0;
  int any = 0;
  while (*p == ' ' || *p == '\t') {
    p++;
  }
  while (*p != '\0' && *p != ' ' && *p != '\t') {
    unsigned d;
    char c = *p;
    if (c >= '0' && c <= '9') {
      d = (unsigned)(c - '0');
    } else if (base == 16u && c >= 'a' && c <= 'f') {
      d = (unsigned)(c - 'a') + 10u;
    } else if (base == 16u && c >= 'A' && c <= 'F') {
      d = (unsigned)(c - 'A') + 10u;
    } else {
      return 1;
    }
    if (d >= base) {
      return 1;
    }
    v = v * base + d;
    any = 1;
    p++;
  }
  *pp = p;
  if (!any) {
    return 1;
  }
  *out = v;
  return 0;
}

/* Skip argv[0] (the module path, possibly quoted). */
static const char *skip_argv0(const char *cmd) {
  if (*cmd == '"') {
    cmd++;
    while (*cmd != '\0' && *cmd != '"') {
      cmd++;
    }
    if (*cmd == '"') {
      cmd++;
    }
  } else {
    while (*cmd != '\0' && *cmd != ' ' && *cmd != '\t') {
      cmd++;
    }
  }
  return cmd;
}

static int parse_args(void) {
  const char *cmd = GetCommandLineA();
  unsigned v;
  if (cmd == NULL) {
    return 0; /* keep defaults */
  }
  cmd = skip_argv0(cmd);
  if (parse_u32(&cmd, 10u, &v) == 0) {
    if (v == 0 || v > MAX_THREADS) {
      return 1;
    }
    g_threads = v;
    if (parse_u32(&cmd, 10u, &v) == 0) {
      if (v == 0) {
        return 1;
      }
      g_iters = v;
      if (parse_u32(&cmd, 16u, &v) == 0) {
        g_phases = v;
      }
    }
  }
  return 0;
}

/* Deterministic per-thread byte pattern; avoids libc memset and keeps each
 * thread's data distinguishable so a cross-thread write is detectable. */
static unsigned char pattern_byte(unsigned id, unsigned index) {
  return (unsigned char)((id * 31u + index * 7u) & 0xffu);
}

/* ---- phases (worker side) ---- */

static void phase_private(unsigned id) {
  volatile unsigned char *buf = g_priv[id];
  unsigned iter, i;
  LONG sum = 0;
  for (iter = 0; iter < g_iters; iter++) {
    for (i = 0; i < PRIV_BYTES; i += 64u) {
      buf[i] = pattern_byte(id, i + iter);
    }
    for (i = 0; i < PRIV_BYTES; i += 64u) {
      sum += (LONG)buf[i];
    }
  }
  g_priv_sum[id] = sum;
}

static void phase_page_excl(unsigned id) {
  volatile unsigned char *page = g_page_excl + ((size_t)id * GUEST_PAGE);
  unsigned iter, i;
  LONG sum = 0;
  for (iter = 0; iter < g_iters; iter++) {
    for (i = 0; i < GUEST_PAGE; i += 8u) {
      page[i] = pattern_byte(id, i + iter);
    }
    for (i = 0; i < GUEST_PAGE; i += 8u) {
      sum += (LONG)page[i];
    }
  }
  g_excl_sum[id] = sum;
}

/* Distinct 8-byte slot inside a single shared page — same-page contention. */
static void phase_page_share(unsigned id) {
  volatile LONG *slot = (volatile LONG *)(g_page_shared + ((size_t)id * 8u));
  unsigned iter;
  for (iter = 0; iter < g_iters; iter++) {
    *slot = (LONG)(iter + 1u);
  }
}

static void phase_interlock(unsigned id) {
  unsigned iter;
  (void)id;
  for (iter = 0; iter < g_iters; iter++) {
    /* Alternate the two entry points so both host handlers are covered. */
    if ((iter & 1u) == 0u) {
      g_inc(&g_interlocked_counter);
    } else {
      g_xadd(&g_interlocked_counter, 1);
    }
  }
}

static void phase_critsec(unsigned id) {
  unsigned iter;
  (void)id;
  for (iter = 0; iter < g_iters; iter++) {
    EnterCriticalSection(&g_cs);
    /* Deliberately a plain read-modify-write: correctness depends entirely on
     * the lock, so a broken CS shows up as a counter mismatch. */
    g_critsec_counter = g_critsec_counter + 1;
    LeaveCriticalSection(&g_cs);
  }
}

static void phase_readers(unsigned id) {
  unsigned iter, i;
  LONG sum = 0;
  for (iter = 0; iter < g_iters; iter++) {
    for (i = 0; i < SHARED_READ_BYTES; i += 256u) {
      sum += (LONG)g_shared_read[i];
    }
  }
  g_read_sum[id] = sum;
}

static void phase_va_churn(unsigned id) {
  unsigned iter;
  unsigned n = g_iters / CHURN_DIVISOR;
  if (n == 0u) {
    n = 1u;
  }
  for (iter = 0; iter < n; iter++) {
    void *p = VirtualAlloc(NULL, CHURN_BYTES, MEM_COMMIT | MEM_RESERVE,
                           PAGE_READWRITE);
    if (p == NULL) {
      g_churn_fail[id] = 1;
      return;
    }
    /* Touch it so the mapping is actually resolved, not just reserved. */
    ((volatile unsigned char *)p)[0] = (unsigned char)iter;
    ((volatile unsigned char *)p)[CHURN_BYTES - 1u] = (unsigned char)id;
    if (!VirtualFree(p, 0, MEM_RELEASE)) {
      g_churn_fail[id] = 1;
      return;
    }
  }
}

static void barrier_wait(void) {
  if (g_inc(&g_barrier_count) == (LONG)g_threads) {
    SetEvent(g_barrier_gate); /* manual reset: releases everyone, stays set */
  }
  WaitForSingleObject(g_barrier_gate, INFINITE);
}

static void phase_pingpong(unsigned id) {
  unsigned iter;
  unsigned n = g_iters / PING_DIVISOR;
  if (n == 0u) {
    n = 1u;
  }
  for (iter = 0; iter < n; iter++) {
    SetEvent(g_ping[id]);
    if (WaitForSingleObject(g_pong[id], 5000) != WAIT_OBJECT_0) {
      return;
    }
    g_ping_count[id] = g_ping_count[id] + 1;
  }
}

static DWORD WINAPI worker(LPVOID param) {
  unsigned id = (unsigned)(ULONG_PTR)param;

  /* All threads block here so each phase starts with real concurrency. */
  WaitForSingleObject(g_start, INFINITE);

  if (g_phases & PH_PRIVATE) {
    phase_private(id);
  }
  if (g_phases & PH_PAGE_EXCL) {
    phase_page_excl(id);
  }
  if (g_phases & PH_PAGE_SHR) {
    phase_page_share(id);
  }
  if (g_phases & PH_INTERLOCK) {
    phase_interlock(id);
  }
  if (g_phases & PH_CRITSEC) {
    phase_critsec(id);
  }
  if (g_phases & PH_READERS) {
    phase_readers(id);
  }
  if (g_phases & PH_VA_CHURN) {
    phase_va_churn(id);
  }
  if (g_phases & PH_PINGPONG) {
    barrier_wait();
    phase_pingpong(id);
  }
  return 0;
}

/* Primary answers every worker's ping so the handshake completes. */
static void drive_pingpong(void) {
  unsigned n = g_iters / PING_DIVISOR;
  unsigned iter, t;
  if (n == 0u) {
    n = 1u;
  }
  /* Wait for every worker to reach the barrier before servicing any ping. */
  WaitForSingleObject(g_barrier_gate, INFINITE);
  for (iter = 0; iter < n; iter++) {
    for (t = 0; t < g_threads; t++) {
      if (WaitForSingleObject(g_ping[t], 5000) != WAIT_OBJECT_0) {
        return;
      }
      SetEvent(g_pong[t]);
    }
  }
}

static LONG expected_private_sum(unsigned id) {
  unsigned iter, i;
  LONG sum = 0;
  for (iter = 0; iter < g_iters; iter++) {
    for (i = 0; i < PRIV_BYTES; i += 64u) {
      sum += (LONG)pattern_byte(id, i + iter);
    }
  }
  return sum;
}

static LONG expected_excl_sum(unsigned id) {
  unsigned iter, i;
  LONG sum = 0;
  for (iter = 0; iter < g_iters; iter++) {
    for (i = 0; i < GUEST_PAGE; i += 8u) {
      sum += (LONG)pattern_byte(id, i + iter);
    }
  }
  return sum;
}

void entry(void) {
  HANDLE threads[MAX_THREADS];
  DWORD tid = 0;
  unsigned t, i;
  HMODULE k;

  if (parse_args() != 0) {
    ExitProcess(20);
  }

  k = GetModuleHandleA("kernel32.dll");
  g_inc = (PFN_InterlockedIncrement)GetProcAddress(k, "InterlockedIncrement");
  g_xadd =
      (PFN_InterlockedExchangeAdd)GetProcAddress(k, "InterlockedExchangeAdd");
  if (g_inc == NULL || g_xadd == NULL) {
    ExitProcess(4);
  }

  InitializeCriticalSection(&g_cs);

  /* Buffers. VirtualAlloc rather than the heap so each region is its own
   * allocation, which is what the region-pin ranking sees. */
  for (t = 0; t < g_threads; t++) {
    g_priv[t] = (volatile unsigned char *)VirtualAlloc(
        NULL, PRIV_BYTES, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE);
    if (g_priv[t] == NULL) {
      ExitProcess(3);
    }
  }
  g_page_excl = (volatile unsigned char *)VirtualAlloc(
      NULL, (size_t)GUEST_PAGE * MAX_THREADS, MEM_COMMIT | MEM_RESERVE,
      PAGE_READWRITE);
  g_page_shared = (volatile unsigned char *)VirtualAlloc(
      NULL, GUEST_PAGE, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE);
  g_shared_read = (volatile unsigned char *)VirtualAlloc(
      NULL, SHARED_READ_BYTES, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE);
  if (g_page_excl == NULL || g_page_shared == NULL || g_shared_read == NULL) {
    ExitProcess(3);
  }
  for (i = 0; i < SHARED_READ_BYTES; i++) {
    g_shared_read[i] = (unsigned char)(i & 0xffu);
  }

  g_start = CreateEventA(NULL, TRUE, FALSE, NULL); /* manual reset */
  g_barrier_gate = CreateEventA(NULL, TRUE, FALSE, NULL); /* manual reset */
  if (g_start == NULL || g_barrier_gate == NULL) {
    ExitProcess(5);
  }
  for (t = 0; t < g_threads; t++) {
    g_ping[t] = CreateEventA(NULL, FALSE, FALSE, NULL);
    g_pong[t] = CreateEventA(NULL, FALSE, FALSE, NULL);
    if (g_ping[t] == NULL || g_pong[t] == NULL) {
      ExitProcess(5);
    }
  }

  for (t = 0; t < g_threads; t++) {
    threads[t] = CreateThread(NULL, 0, worker, (LPVOID)(ULONG_PTR)t, 0, &tid);
    if (threads[t] == NULL || threads[t] == INVALID_HANDLE_VALUE) {
      ExitProcess(1);
    }
  }

  SetEvent(g_start); /* release all workers at once */

  if (g_phases & PH_PINGPONG) {
    drive_pingpong();
  }

  for (t = 0; t < g_threads; t++) {
    if (WaitForSingleObject(threads[t], INFINITE) != WAIT_OBJECT_0) {
      ExitProcess(2);
    }
  }

  /* ---- verification ---- */

  if (g_phases & PH_PRIVATE) {
    for (t = 0; t < g_threads; t++) {
      if (g_priv_sum[t] != expected_private_sum(t)) {
        ExitProcess(10);
      }
    }
  }
  if (g_phases & PH_PAGE_EXCL) {
    for (t = 0; t < g_threads; t++) {
      if (g_excl_sum[t] != expected_excl_sum(t)) {
        ExitProcess(11);
      }
    }
    /* Each thread must have written only its own page. */
    for (t = 0; t < g_threads; t++) {
      volatile unsigned char *page = g_page_excl + ((size_t)t * GUEST_PAGE);
      if (page[0] != pattern_byte(t, 0u + (g_iters - 1u))) {
        ExitProcess(11);
      }
    }
  }
  if (g_phases & PH_PAGE_SHR) {
    for (t = 0; t < g_threads; t++) {
      volatile LONG *slot = (volatile LONG *)(g_page_shared + ((size_t)t * 8u));
      if (*slot != (LONG)g_iters) {
        ExitProcess(12);
      }
    }
  }
  if (g_phases & PH_INTERLOCK) {
    if (g_interlocked_counter != (LONG)(g_threads * g_iters)) {
      ExitProcess(13);
    }
  }
  if (g_phases & PH_CRITSEC) {
    if (g_critsec_counter != (LONG)(g_threads * g_iters)) {
      ExitProcess(14);
    }
  }
  if (g_phases & PH_READERS) {
    LONG expect = 0;
    for (i = 0; i < SHARED_READ_BYTES; i += 256u) {
      expect += (LONG)(unsigned char)(i & 0xffu);
    }
    expect *= (LONG)g_iters;
    for (t = 0; t < g_threads; t++) {
      if (g_read_sum[t] != expect) {
        ExitProcess(15);
      }
    }
  }
  if (g_phases & PH_VA_CHURN) {
    for (t = 0; t < g_threads; t++) {
      if (g_churn_fail[t] != 0) {
        ExitProcess(16);
      }
    }
  }
  if (g_phases & PH_PINGPONG) {
    unsigned n = g_iters / PING_DIVISOR;
    if (n == 0u) {
      n = 1u;
    }
    for (t = 0; t < g_threads; t++) {
      if (g_ping_count[t] != (LONG)n) {
        ExitProcess(17);
      }
    }
  }

  DeleteCriticalSection(&g_cs);
  ExitProcess(0);
}
