/*
 * Micro-PE: a worker thread dies on an invalid guest access while the primary
 * still reaches `ExitProcess(0)`.
 *
 * This exists to witness one behaviour: a guest can report a *successful* exit
 * while a thread it created has already been killed by an invalid access. On
 * `mt_contention 4 64 40` that happened in 12 of 14 faulting runs, which meant a
 * suite could pass while workers died. `wie-runtime`'s `run_micro_exe_with_options`
 * now turns `ExitProcess { code: 0 }` *plus* a recorded worker fault into a
 * `RuntimeStop(...)` naming the fault; the test that consumes this binary fails if
 * that branch is deleted, which is the only way to know it works rather than merely
 * compiles.
 *
 * The fault is deliberate and unmapped on purpose: a wild store the guest neither
 * catches (no SEH frame is installed) nor checks afterwards. The primary must not
 * notice — it waits on the thread handle, then exits 0.
 *
 * Exit codes (the guest's own verdict, which must be *masked* by the fault):
 *   0 — the primary did its whole job
 *   1 — CreateThread failed
 *   2 — WaitForSingleObject failed
 *   3 — the worker was still running after its wait returned
 */

#include <windows.h>

/* Far outside any image, stack or heap WIE hands out, and page-aligned so the
 * fault is unambiguous: address 0 can be a null-deref special case, and 0x1000 is
 * the null-base corruption the 7za record documents. */
#define BAD_ADDR ((volatile unsigned long long *)0x7E5700000ULL)

static volatile LONG g_ready = 0;

static DWORD WINAPI worker(LPVOID param) {
    (void)param;
    g_ready = 1;
    /* The whole point: an access the guest cannot survive and does not trap. */
    *BAD_ADDR = 0xDEADBEEFCAFEF00DULL;
    /* Unreachable when the fault is delivered. */
    ExitThread(0);
    return 0;
}

void entry(void) {
    HANDLE h;
    DWORD tid = 0;
    DWORD wait;
    DWORD code = 0;

    h = CreateThread(NULL, 0, worker, NULL, 0, &tid);
    if (h == NULL || h == INVALID_HANDLE_VALUE) {
        ExitProcess(1);
    }
    wait = WaitForSingleObject(h, 2000);
    if (wait != WAIT_OBJECT_0) {
        ExitProcess(2);
    }
    if (!GetExitCodeThread(h, &code)) {
        ExitProcess(3);
    }
    /* STILL_ACTIVE means the handle signalled without the thread ending. */
    if (code == 0x103) {
        ExitProcess(3);
    }
    /* A worker died; the guest says everything is fine anyway. */
    ExitProcess(0);
}
