#!/usr/bin/env bash
# Build the DriverKit extension, or the host tests for the parser it shares.
#
#   ./build.sh test    parser tests, no Xcode or entitlements needed
#   ./build.sh probe   run the parser over attached hardware
#   ./build.sh dext    compile and link DsdAudioDriver.dext
#
# Signing and installing are separate, and need an entitlement grant from Apple. See
# README.md.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
build="${here}/build"
src="${here}/DsdAudioDriver"

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

build_dext() {
  if [ ! -d "${sdk}" ]; then
    echo "no DriverKit SDK at ${sdk}" >&2
    echo "full Xcode is needed; run: sudo xcode-select -s /Applications/Xcode.app" >&2
    exit 1
  fi
  local gen="${build}/gen"
  local genhdr="${gen}/DsdAudioDriver"
  local bundle="${build}/DsdAudioDriver.dext"
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
  echo "sign it with your team's DriverKit provisioning profile before installing:"
  echo "  codesign --force --sign <identity> \\"
  echo "    --entitlements ${src}/DsdAudioDriver.entitlements \\"
  echo "    ${bundle}"
}

case "${1:-test}" in
  test) run_tests ;;
  probe) run_probe ;;
  dext) build_dext ;;
  *) echo "usage: $0 [test|probe|dext]" >&2; exit 2 ;;
esac
