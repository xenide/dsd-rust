#!/usr/bin/env bash
# Build the DriverKit extension, its installer app, or the host tests.
#
#   ./build.sh test    parser tests, no Xcode or entitlements needed
#   ./build.sh probe   run the parser over attached hardware
#   ./build.sh dext    compile and link the driver extension
#   ./build.sh app     assemble and ad-hoc sign the installer app around the dext
#
# Loading what `app` produces needs a machine with SIP off and AMFI and the DriverKit
# entitlement checks disabled. README.md has the procedure and the reasons.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
build="${here}/build"
src="${here}/DsdAudioDriver"

# The system copies the dext out of the app by bundle identifier, so the bundle has to be
# named for it. Anything else fails to install with no useful message.
driver_id="com.github.xenide.dsdrust.driver"
app_name="DsdDriverInstaller"

xcode="$(xcode-select -p)"
toolchain="${xcode}/Toolchains/XcodeDefault.xctoolchain/usr/bin"
sdk="${xcode}/Platforms/DriverKit.platform/Developer/SDKs/DriverKit.sdk"

run_tests() {
  mkdir -p "${build}"
  clang++ -std=c++17 -Wall -Wextra -Werror \
    -I"${here}" -I"${src}" \
    -o "${build}/uac2_test" \
    "${here}/tests/test_dsd_uac2.cpp" "${src}/DsdUac2.cpp"
  "${build}/uac2_test"
}

run_probe() {
  mkdir -p "${build}"
  clang++ -std=c++17 -Wall -Wextra -Werror \
    -I"${src}" \
    -framework IOKit -framework CoreFoundation \
    -o "${build}/probe" \
    "${here}/tools/probe.cpp" "${src}/DsdUac2.cpp"
  "${build}/probe"
}

require_sdk() {
  if [ -d "${sdk}" ]; then
    return
  fi
  echo "no DriverKit SDK at ${sdk}" >&2
  echo "full Xcode is needed; run: sudo xcode-select -s /Applications/Xcode.app" >&2
  exit 1
}

build_dext() {
  require_sdk
  local gen="${build}/gen"
  local genhdr="${gen}/DsdAudioDriver"
  local bundle="${build}/${driver_id}.dext"
  rm -rf "${bundle}"
  mkdir -p "${genhdr}" "${bundle}/Contents/MacOS"

  # iig turns the .iig into the header the driver includes and the dispatch glue it needs.
  "${toolchain}/iig" \
    --def "${src}/DsdAudioDriver.iig" \
    --header "${genhdr}/DsdAudioDriver.h" \
    --impl "${gen}/DsdAudioDriver.iig.cpp" \
    --deployment-target 21.0 \
    --framework-name DsdAudioDriver \
    -- -isysroot "${sdk}" -x c++ -std=gnu++17 -D__IIG=1 -DDRIVERKIT=1 \
    -I"${gen}" -I"${src}" -I"${sdk}/System/DriverKit/usr/include" \
    -F"${sdk}/System/DriverKit/System/Library/Frameworks"

  local flags=(
    -isysroot "${sdk}"
    -target arm64-apple-driverkit21.0
    -std=gnu++17 -fno-exceptions -fno-rtti -fbuiltin
    -Wall -Wextra -Werror -Wno-unused-parameter
    -I"${gen}" -I"${src}"
    -F"${sdk}/System/DriverKit/System/Library/Frameworks"
  )
  rm -rf "${build}/obj"
  mkdir -p "${build}/obj"
  for source in "${src}/DsdAudioDriver.cpp" "${src}/DsdUac2.cpp" "${gen}/DsdAudioDriver.iig.cpp"; do
    "${toolchain}/clang++" "${flags[@]}" -c "${source}" \
      -o "${build}/obj/$(basename "${source}").o"
  done

  "${toolchain}/clang++" \
    -isysroot "${sdk}" -target arm64-apple-driverkit21.0 \
    -F"${sdk}/System/DriverKit/System/Library/Frameworks" \
    -L"${sdk}/System/DriverKit/usr/lib" \
    -framework DriverKit -framework USBDriverKit -framework AudioDriverKit \
    -o "${bundle}/Contents/MacOS/DsdAudioDriver" \
    "${build}/obj"/*.o

  cp "${src}/Info.plist" "${bundle}/Contents/Info.plist"
  echo "built ${bundle}"
}

build_app() {
  build_dext
  local app="${build}/${app_name}.app"
  local dext="${build}/${driver_id}.dext"
  rm -rf "${app}"
  mkdir -p "${app}/Contents/MacOS" "${app}/Contents/Library/SystemExtensions"

  xcrun swiftc -O \
    -target arm64-apple-macos13.0 \
    -o "${app}/Contents/MacOS/${app_name}" \
    "${here}/installer/main.swift"
  cp "${here}/installer/Info.plist" "${app}/Contents/Info.plist"
  cp -R "${dext}" "${app}/Contents/Library/SystemExtensions/"

  # Signing inside out: the system checks the nested dext's signature as part of the app's.
  codesign --force --sign - --timestamp=none \
    --entitlements "${src}/DsdAudioDriver.entitlements" \
    "${app}/Contents/Library/SystemExtensions/${driver_id}.dext"
  codesign --force --sign - --timestamp=none \
    --entitlements "${here}/installer/${app_name}.entitlements" \
    "${app}"

  echo "built ${app}"
  echo
  echo "ad-hoc signed, so it loads only on a machine with the checks off:"
  echo "  csrutil status            -> disabled"
  echo "  nvram boot-args           -> amfi_get_out_of_my_way=1 dk=0x8001"
  echo "  systemextensionsctl list  -> developer mode on"
  echo
  echo "then:  ${app}/Contents/MacOS/${app_name} activate"
}

case "${1:-test}" in
  test) run_tests ;;
  probe) run_probe ;;
  dext) build_dext ;;
  app) build_app ;;
  *) echo "usage: $0 [test|probe|dext|app]" >&2; exit 2 ;;
esac
