/*
 * Micro-PE WINMM: timeGetTime + timeSetEvent + the waveOut stubs.
 *
 * CRT-linked console program (printf) — see micro-exes/ws2_echo/main.c.
 * Exercises the host's WINMM handlers (crates/wie-winapi/src/winmm.rs): the
 * fake wave-out device (open/close round-trip) and the stored multimedia
 * timer (timeSetEvent → timeKillEvent round-trip). No callback is registered
 * (lpFunc = NULL), so the guest never waits on a host timer.
 *
 * Exit codes:
 *   0 — ok
 *   1 — timeGetTime() == 0
 *   2 — waveOutGetNumDevs() < 1
 *   3 — waveOutOpen failed or returned a zero handle
 *   4 — waveOutClose failed
 *   5 — timeSetEvent returned 0
 *   6 — timeKillEvent returned TIMERR_NOCANDO
 */

#include <windows.h>
#include <mmsystem.h>
#include <stdio.h>

int main(void) {
    DWORD t;

    printf("winmm_audio: timeGetTime()...\n");
    t = timeGetTime();
    if (t == 0) {
        printf("  FAILED (timeGetTime == 0)\n");
        return 1;
    }
    printf("  ok (%lu ms)\n", (unsigned long)t);

    printf("winmm_audio: waveOutGetNumDevs()...\n");
    {
        UINT n = waveOutGetNumDevs();
        if (n < 1) {
            printf("  FAILED (num devs %u)\n", (unsigned)n);
            return 2;
        }
        printf("  ok (%u)\n", (unsigned)n);
    }

    printf("winmm_audio: waveOutOpen(WAVE_MAPPER)...\n");
    {
        HWAVEOUT hwo = NULL;
        MMRESULT rc = waveOutOpen(&hwo, WAVE_MAPPER, NULL, 0, 0, 0);
        if (rc != MMSYSERR_NOERROR || hwo == NULL) {
            printf("  FAILED (rc %u, hwo %p)\n", (unsigned)rc, (void *)hwo);
            return 3;
        }
        printf("  ok (hwo=%p)\n", (void *)hwo);

        if (waveOutClose(hwo) != MMSYSERR_NOERROR) {
            printf("  FAILED (waveOutClose)\n");
            return 4;
        }
        printf("  ok (closed)\n");
    }

    printf("winmm_audio: timeSetEvent(100, 10, NULL, 0, TIME_ONESHOT)...\n");
    {
        UINT id = timeSetEvent(100, 10, NULL, 0, TIME_ONESHOT);
        if (id == 0) {
            printf("  FAILED (timeSetEvent == 0)\n");
            return 5;
        }
        printf("  ok (id=0x%08lX)\n", (unsigned long)id);

        if (timeKillEvent(id) != TIMERR_NOERROR) {
            printf("  FAILED (timeKillEvent != TIMERR_NOERROR)\n");
            return 6;
        }
        printf("  ok (killed)\n");
    }

    printf("winmm_audio: done\n");
    return 0;
}
