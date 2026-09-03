# DriverKit extension

A DriverKit extension that matches the DAC's USB audio streaming interface directly, so
`usbaudiod` never gets it.

This is item C1 of [issue #8](https://github.com/xenide/dsd-rust/issues/8). The CLI's answer
to the same problem is to force a re-enumeration and race the daemon for the window that
follows (`src/output/usb/device.rs`). A dext removes the race rather than winning it: IOKit
gives an interface to one driver, so matching it excludes the daemon by construction. Two
things follow that the CLI cannot have.

**Native DSD becomes a system output device.** Core Audio will not select a UAC2 alternate
setting whose format is `RAW_DATA`, which is why the native path is unreachable through the
HAL. A driver that owns the interface selects it itself, and publishes it as an ordinary
output device that any application can open.

**The DAC's clock drives the timeline.** The driver posts zero timestamps from isochronous
completions, so Core Audio follows the DAC rather than the two running open loop. The CLI
tracks the DAC's feedback endpoint to correct drift instead; here there is nothing to
correct.

## Status

The dext compiles and links against the DriverKit 25.5 SDK, and every symbol it imports is
exported by the SDK. It has never been loaded, because loading it needs entitlements Apple
grants case by case (below) and a DAC to load it against. Treat the data path as unverified.

The descriptor parser is the part that carries the domain knowledge, and it has no DriverKit
dependency for exactly that reason: `./build.sh test` builds and runs its tests on the host,
with no Xcode, no entitlements and no DAC. Those tests are held to the same Cayin RU7
configuration descriptor as the Rust parser in `src/output/usb/descriptors.rs`.

## Layout

| file | what it is |
| --- | --- |
| `DsdAudioDriver/DsdUac2.{h,cpp}` | UAC2 descriptor parsing and the format list. No DriverKit. |
| `DsdAudioDriver/DsdAudioDriver.iig` | The driver class, as iig reads it. |
| `DsdAudioDriver/DsdAudioDriver.cpp` | Matching, the audio objects, and the isochronous engine. |
| `DsdAudioDriver/Info.plist` | The matching personality. |
| `DsdAudioDriver/DsdAudioDriver.entitlements` | What Apple has to grant. |
| `tests/test_dsd_uac2.cpp` | Host tests for the parser. |

## Building

```
./build.sh test    # parser tests, nothing else needed
./build.sh dext    # compile and link build/DsdAudioDriver.dext
```

`dext` needs full Xcode, not Command Line Tools:

```
sudo xcode-select -s /Applications/Xcode.app
```

## Before it can load

Three things, in order, and the first is the long pole.

**1. Entitlements from Apple.** `com.apple.developer.driverkit.transport.usb` and
`com.apple.developer.driverkit.family.audio` are granted case by case, on request through the
Apple Developer account, and the request takes weeks. `DsdAudioDriver.entitlements` is what to
ask for. Nothing below works until this lands, so start it before writing any more code.

**2. Fill in the DAC.** `Info.plist` and the entitlements both carry `idVendor` and
`idProduct` set to zero. Fill both in, in decimal, from `dsd-rust devices` or
`ioreg -p IOUSB -l`. They are deliberately not wildcards: a personality matching every UAC2
streaming interface would displace `usbaudiod` for every USB audio device on the machine.

**3. Ship it in an app.** A dext installs only from inside a notarized app bundle, through
`OSSystemExtensionRequest`, with the user approving it in System Settings. The app is not in
this repository. For development, `systemextensionsctl developer on` relaxes the notarization
requirement but not the entitlement one.

## Notes for whoever picks this up

**DSD silence is not zero.** Native DSD goes out as big-endian 32-bit integer PCM, marked
non-mixable so the HAL leaves the bits alone. Core Audio does not know the stream is DSD, so
anything writing zeroes into it writes a signal that drops the DAC out of lock. The driver
prefills its ring with `0x69` so the gap before the first write is silent, but an application
has to keep writing DSD silence rather than PCM silence when it has nothing to play.

**The frame list is per microframe.** `IsochIO` takes one `IOUSBIsochronousFrame` per service
interval, and the data buffer is packed: each microframe starts where the one before it
ended. This matches what `src/output/usb/stream.rs` does through the older IOKit API.

**Rates come from the clock, not from Core Audio.** The driver reads the UAC2 clock `RANGE`
report and builds its format list from that, rather than inheriting the intersection Core
Audio derives, which is sometimes narrower. This is the same reading that item A1 of issue #8
added to the CLI.

**What is untested.** Everything past `Start`: alternate setting selection, the clock rate
request, transfer submission, and the timestamps. The first run against real hardware should
expect to find bugs there.

One to look at first: completions arrive on their own dispatch queue, and
`UpdateCurrentZeroTimestamp` is called from there rather than from the device's work queue.
Apple's own samples post timestamps from the work queue. If timing misbehaves, that is the
first thing to change.
