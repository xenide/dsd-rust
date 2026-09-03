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

The driver loads, matches a Cayin RU7, and publishes it as a Core Audio device. On a machine
with the checks off (below), against real hardware:

```
DsdAudioDriver: 32 formats over 4 alternate settings
DsdAudioDriver: published, registering
DK: DsdAudioDriver::start(Cayin RU7) ok
```

```
+-o Cayin RU7 <IOUSBHostDevice>
  +-o DsdAudioDriver          <IOUserService, registered, matched, active>
  +-o Cayin RU7@0             <IOUSBHostInterface, !registered, !matched>
  +-o Cayin RU7 Playback@1    <IOUSBHostInterface, !registered, !matched>
```

Both interfaces sit unmatched under the driver, which is the point: `usbaudiod` never sees
them. The DAC then appears to every application, `dsd-rust devices` included, under the
driver's own UID rather than `AppleUSBAudioEngine`.

What is still unexercised is the isochronous data path. Nothing has been played through the
driver: `StartDevice`, the alternate setting selection, the clock rate request and transfer
submission have never run.

The descriptor parser is the part that carries the domain knowledge, and it has no DriverKit
dependency for exactly that reason. It is checked two ways, neither of which needs the dext
to load:

- `./build.sh test` runs its tests on the host, with no Xcode, no entitlements and no DAC,
  against the same Cayin RU7 configuration descriptor the Rust parser in
  `src/output/usb/descriptors.rs` is held to.
- `./build.sh probe` runs it over whatever is plugged in, reading the real configuration
  descriptor and the real clock `RANGE` report. Against an RU7 it finds alternate setting 4
  as `RAW_DATA`, endpoint 0x01 with feedback on 0x81, a 776 byte packet, and the eight clock
  rates from 44100 to 384000 that `dsd-rust devices` reports on its `native` line.

## Layout

| file | what it is |
| --- | --- |
| `DsdAudioDriver/DsdUac2.{h,cpp}` | UAC2 descriptor parsing and the format list. No DriverKit. |
| `DsdAudioDriver/DsdAudioDriver.iig` | The driver class, as iig reads it. |
| `DsdAudioDriver/DsdAudioDriver.cpp` | Matching, the audio objects, and the isochronous engine. |
| `DsdAudioDriver/Info.plist` | The matching personality. |
| `DsdAudioDriver/DsdAudioDriver.entitlements` | What Apple has to grant. |
| `tests/test_dsd_uac2.cpp` | Host tests for the parser. |
| `tools/probe.cpp` | Runs the parser over attached hardware. |
| `installer/` | The app that activates the dext, and its entitlement. |

## Building

```
./build.sh test    # parser tests, nothing else needed
./build.sh probe   # run the parser over attached hardware
./build.sh dext    # compile and link the driver extension
./build.sh app     # assemble and ad-hoc sign the installer app around it
```

`dext` needs full Xcode, not Command Line Tools:

```
sudo xcode-select -s /Applications/Xcode.app
```

## What has to be true before it loads

A dext never loads on its own. It ships inside an app bundle, named for its own bundle
identifier, and that app asks the system to activate it through `OSSystemExtensionRequest`.
`installer/` is that app, and `./build.sh app` assembles the two together and ad-hoc signs
them.

What stops an ad-hoc signed dext is the entitlements. Every DriverKit entitlement is
restricted, and AMFI kills any process at launch that claims a restricted entitlement its
signature does not authorise. That is easy to see directly:

| the installer, ad-hoc signed | result |
| --- | --- |
| with `com.apple.developer.system-extension.install` | killed at launch, exit 137 |
| with entitlements stripped | runs, exits 2 on a bad argument |

There are two ways past it.

### The supported way: a development-signed build

Contrary to what the older Apple documentation implies, **the DriverKit entitlements this
driver needs are self-serve for development**. From the WWDC22 DriverKit session, on the
audio family entitlement: "This new entitlement is public for development, so you can get
started using this today without filing a request. In fact, all DriverKit family entitlements
are now available to use for development." The request form is for *distribution*.

What that path needs is an Apple Developer Program membership, for an Apple Development
signing certificate and development provisioning profiles. In the App ID editor, enable
`DriverKit (development)`, `DriverKit USB Transport (development)` and `DriverKit Family
Audio` on the driver's App ID, and the System Extension capability on the app's. SIP stays
on, and no boot arguments are needed.

The development USB transport entitlement takes `idVendor` as a wildcard rather than a
number, which is not what `DsdAudioDriver.entitlements` carries. Widen it for development and
narrow it again to ship.

### The unsupported way: turn the checks off

No developer account, and the checks come off the machine instead. This is not a supported
configuration, it leaves the Mac meaningfully less secure, and it may simply stop working on
a future macOS. Every step is reversible.

Apple silicon gates boot arguments at three layers, so all three come off. Shut down fully,
then hold the power button until "Loading startup options" appears, click Options, Continue,
and sign in. From the menu bar open Utilities > Terminal, and run, in this order:

```
bputil -a          # lift iBoot's boot-args allowlist; downgrades to Permissive Security
csrutil disable    # SIP, which also guards NVRAM
```

`bputil` goes first because it rewrites the boot policy. Reboot into macOS and set the two
kernel flags:

```
sudo nvram boot-args="amfi_get_out_of_my_way=1 dk=0x8001"
```

`amfi_get_out_of_my_way=1` stops AMFI killing the installer over its entitlement. `dk` is
DriverKit's own bitfield: `0x1` keeps DriverKit enabled at all and `0x8000` turns off its
entitlement checks, so `0x8001` is both. If `nvram` answers "not permitted", the write is
going to the wrong store; address it explicitly:

```
sudo nvram 40A0DDD2-77F8-4392-B4A3-1E7304206516:boot-args="amfi_get_out_of_my_way=1 dk=0x8001"
```

Reboot again, then check the machine is in the state you asked for, and let system extensions
load from outside `/Applications`:

```
csrutil status                     # disabled
nvram boot-args                    # both flags
sudo systemextensionsctl developer on
```

Now build and activate:

```
./build.sh app
build/DsdDriverInstaller.app/Contents/MacOS/DsdDriverInstaller activate
systemextensionsctl list
```

Approve the extension in System Settings > General > Login Items & Extensions if asked. When
activation fails it says so with an `OSSystemExtensionError` code, and `sysextd` logs the
reason:

```
log show --last 2m --predicate 'subsystem == "com.apple.sysextd"'
```

To undo everything: `DsdDriverInstaller deactivate`, then `sudo nvram -d boot-args`, then
back in recovery `csrutil enable` and Startup Security Utility set to Full Security.

### Point it at your DAC

`Info.plist` and the entitlements both name the Cayin RU7, `idVendor` 11655 and `idProduct`
49154. Change both for another DAC; `./build.sh probe` prints the decimal pair to use. They
are deliberately not wildcards: a personality matching every UAC2 streaming interface would
displace `usbaudiod` for every USB audio device on the machine.

The dext bundle is named for its bundle identifier, `com.github.xenide.dsdrust.driver.dext`,
because the system copies it out of the app by that name. Renaming it breaks installation
with no useful message, and a bundle identifier over 63 characters fails the same way.

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

**Match the device, not the interface.** macOS will not hand a third party driver a USB
audio class interface; `usbaudiod` is published against those before anyone else can match
them, and a personality with `IOProviderClass` of `IOUSBHostInterface` is simply never
considered. The driver matches `IOUSBHostDevice` instead and calls
`SetConfiguration(value, false)`. Not registering the interfaces for matching is what
excludes the daemon: the nubs exist for `CopyInterface` to hand back, but nothing else ever
sees them published.

**Build for arm64e.** Dexts on Apple silicon are arm64e, not arm64. A plain arm64 binary
stages and enables without complaint and then fails to launch with `Exec format error`, which
reaches the log as a matching failure rather than a link one.

**Two plist keys are load bearing.** `IOUserAudioDriverUserClientProperties` is what lets the
Core Audio host open the driver's user client; without it the driver starts, publishes its
device, and no application ever sees it. `SetDispatchQueue` only accepts a name some method
declares with `QUEUENAME`, so completions run on the default queue, which is also where
Apple's samples post timestamps from.

**A crashing dext panics the machine.** DriverKit restarts a driver that dies, and after a
few restarts the kernel gives up and panics: `Driver IOUserServer(...) has crashed too many
times`. So an assert or a null dereference on the IO path is not a crash to iterate on, it is
a reboot. `OSAction::GetReference` is a live example -- it asserts rather than returning null
when the action carries no reference storage, which is the case for the action a pipe hands
back to an isochronous completion. Guard the IO path and identify transfers by comparing
action pointers rather than through a reference.

**A failed start pins the old copy.** A dext whose `Start` returns an error leaves its server
process running, and that holds the previous staged bundle, so the next `activate` lands in
`terminating for upgrade via delegate` and the kernel tries to launch a bundle that is being
deleted. `pkill -f "SystemExtensions.*DsdAudioDriver"` between iterations avoids a confusing
half hour.

**What is untested.** Everything past `Start`: alternate setting selection, the clock rate
request, transfer submission, and the timestamps.
