/*
 * Micro-PE: the SETUPAPI device-list flow — SetupDiGetClassDevsW →
 * SetupDiEnumDeviceInfo → SetupDiDestroyDeviceInfoList.
 *
 * CRT-linked console program. WIE has no host devices: the enumeration must
 * come back empty and the fake HDEVINFO handle must be destroyable.
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
#include <stdio.h>

int main(void) {
  HDEVINFO dev_info_set;
  SP_DEVINFO_DATA dev_info_data;

  printf("setupapi_list: SetupDiGetClassDevsW...\n");
  dev_info_set = SetupDiGetClassDevsW(NULL, NULL, NULL, 0);
  if (dev_info_set == INVALID_HANDLE_VALUE) {
    printf("  FAILED\n");
    return 1;
  }
  printf("  ok\n");

  printf("setupapi_list: SetupDiEnumDeviceInfo...\n");
  dev_info_data.cbSize = sizeof(SP_DEVINFO_DATA);
  if (SetupDiEnumDeviceInfo(dev_info_set, 0, &dev_info_data) != 0) {
    printf("  FAILED — expected empty list\n");
    return 2;
  }
  printf("  ok (empty — no host devices)\n");

  printf("setupapi_list: SetupDiDestroyDeviceInfoList...\n");
  if (!SetupDiDestroyDeviceInfoList(dev_info_set)) {
    printf("  FAILED\n");
    return 3;
  }
  printf("  ok\n");

  printf("setupapi_list: done\n");
  return 0;
}
