/*
 * Micro-PE: the version.dll flow — GetFileVersionInfoSizeW →
 * GetFileVersionInfoW → VerQueryValueW against the RT_VERSION resource of the
 * running exe itself (built from version.rc).
 *
 * Requires a bottle root (WIE_ROOT / --root), like every file op.
 *
 * Exit codes:
 *   0  — ok
 *   1  — GetFileVersionInfoSizeW failed / size out of range
 *   2  — GetFileVersionInfoW failed
 *   3  — root "\" query failed or wrong size
 *   4  — fixed info signature mismatch
 *   5  — dwFileVersionMS != 1.2
 *   6  — dwFileVersionLS != 3.4
 *   7  — StringFileInfo FileVersion query failed
 *   8  — FileVersion string != "1.2.3.4" / wrong length
 *   9  — VarFileInfo Translation query failed
 *   10 — translation pair != (0x0409, 0x04B0)
 *
 * Docs: GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW
 * (Microsoft Learn). Clean room.
 */

#include <windows.h>

/* The version block of this exe is a few hundred bytes; a static buffer keeps
 * the micro freestanding (no heap). */
static unsigned char version_buf[2048];

static int wcseq(const wchar_t *a, const wchar_t *b) {
  while (*a && *b) {
    if (*a != *b) return 0;
    a++;
    b++;
  }
  return *a == *b;
}

void entry(void) {
  DWORD size;
  DWORD handle = 0;
  BOOL ok;
  void *value;
  UINT len;
  VS_FIXEDFILEINFO *fixed_info;
  const unsigned short *lang_pair;

  size = GetFileVersionInfoSizeW(L"version_query.exe", &handle);
  if (size == 0 || size > sizeof(version_buf)) {
    ExitProcess(1);
  }

  ok = GetFileVersionInfoW(L"version_query.exe", 0, size, version_buf);
  if (!ok) {
    ExitProcess(2);
  }

  /* "\" → VS_FIXEDFILEINFO: file version 1.2.3.4. */
  ok = VerQueryValueW(version_buf, L"\\", &value, &len);
  if (!ok || len != sizeof(VS_FIXEDFILEINFO)) {
    ExitProcess(3);
  }
  fixed_info = (VS_FIXEDFILEINFO *)value;
  if (fixed_info->dwSignature != 0xFEEF04BD) {
    ExitProcess(4);
  }
  if (fixed_info->dwFileVersionMS != 0x00010002) {
    ExitProcess(5);
  }
  if (fixed_info->dwFileVersionLS != 0x00030004) {
    ExitProcess(6);
  }

  /* StringFileInfo FileVersion string == "1.2.3.4" (16 bytes incl. NUL). */
  ok = VerQueryValueW(version_buf, L"\\StringFileInfo\\040904b0\\FileVersion",
                      &value, &len);
  if (!ok || len != 16) {
    ExitProcess(7);
  }
  if (!wcseq((const wchar_t *)value, L"1.2.3.4")) {
    ExitProcess(8);
  }

  /* VarFileInfo Translation: the first pair is (0x0409, 0x04B0). */
  ok = VerQueryValueW(version_buf, L"\\VarFileInfo\\Translation", &value, &len);
  if (!ok || len < 4) {
    ExitProcess(9);
  }
  lang_pair = (const unsigned short *)value;
  if (lang_pair[0] != 0x0409 || lang_pair[1] != 0x04B0) {
    ExitProcess(10);
  }

  ExitProcess(0);
}
