/*
 * Micro-PE: the SETUPAPI device-list flow — SetupDiGetClassDevsW →
 * SetupDiEnumDeviceInfo → SetupDiDestroyDeviceInfoList.
 *
 * WIE has no host devices: the enumeration must come back empty and the
 * fake HDEVINFO handle must be destroyable.
 *
 * Exit codes:
 *   0  — ok
 *   1  — SetupDiGetClassDevsW returned INVALID_HANDLE_VALUE
 *   2  — SetupDiEnumDeviceInfo returned nonzero (list should be empty)
 *   3  — SetupDiDestroyDeviceInfoList returned zero
 *
 * Docs: SetupDiGetClassDevsW, SetupDiEnumDeviceInfo,
 * SetupDiDestroyDeviceInfoList (Microsoft Learn). Clean room.
 */

#include <windows.h>
#include <setupapi.h>

void entry(void) {
  HDEVINFO dev_info_set;
  SP_DEVINFO_DATA dev_info_data;

  dev_info_set = SetupDiGetClassDevsW(NULL, NULL, NULL, 0);
  if (dev_info_set == INVALID_HANDLE_VALUE) {
    ExitProcess(1);
  }

  dev_info_data.cbSize = sizeof(SP_DEVINFO_DATA);
  if (SetupDiEnumDeviceInfo(dev_info_set, 0, &dev_info_data) != 0) {
    ExitProcess(2);
  }

  if (!SetupDiDestroyDeviceInfoList(dev_info_set)) {
    ExitProcess(3);
  }

  ExitProcess(0);
}
