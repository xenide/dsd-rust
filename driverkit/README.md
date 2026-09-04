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

Audio plays, as PCM and as native DSD. A file played to the DAC through the driver comes out
of it, continuously, with no clock resets from Core Audio.

```
DsdAudioDriver: host asked for 44100 Hz PCM, alternate setting 1, in-flight window 1411 frames
DsdAudioDriver: host wrote 512 frames at sample 521028 (ring slot 836); engine reads at sample 520556 (slot 364)
```

```
DsdAudioDriver: host asked for 352800 Hz native DSD, alternate setting 4, in-flight window 11289 frames
DsdAudioDriver: streaming 352800 Hz on alt 4: ring 32768 frames of 8 bytes, timeline resumes at 10431513, feedback endpoint 0x81 pipe open
```

Native DSD is what the alternate setting exists for, and it is what Core Audio cannot reach
on its own. `dsd-rust play` takes it whenever the file's rate has no PCM carrier: DSD256
needs a 705600 Hz DoP carrier the DAC does not offer, and goes down alternate setting 4 at
352800 frames a second instead.

**The timestamp period is sized for the fastest rate, not the slowest.** Two things scale
with it and both get worse as the rate rises. The host writes a safety offset ahead of the
timeline while the driver reads an in-flight window behind it, so the pair sit more than
twice that window apart in a ring exactly one period long: at 4096 frames and 352800 Hz they
wrapped past each other several times a second. And a timestamp has to land on an exact
multiple of the period, interpolated between two isochronous completions, so a period
covering few transfers inherits the jitter of individual ones -- three transfers at 352800
against twenty-three at 44100, and Core Audio read a rate 7% out and spent a minute and a
half walking back from it. 32768 frames covers twenty-three transfers at 352800 again.

**Core Audio's timeline outlives an IO stop, so the driver's has to as well.** Its sample time
carries across a stop and start, and its counter follows the timeline the driver posts rather
than restarting alongside it: the sample time the host writes at on the first cycle of a track
is the number of frames the track before it played. A driver that zeroes its own counter each
time anchors the two to different timelines, so the ring is read nowhere near where the host
writes. The HAL then walks the difference off at a few thousand frames a second, crossing the
write point every few seconds for the minute or more that takes, which is audible as a sandy
noise that comes and goes. It sounds like drift and is not: the rate is right throughout.

`IOUserAudioDevice` inherits `GetCurrentZeroTimestamp` from `IOUserAudioClockDevice`, so
`StartIsoc` reads back the last timestamp posted and starts there. It is already a multiple of
the period, and it reads back zero before the first track. Resuming leaves the two aligned from
the first transfer, and the margin between them is then the in-flight window it was meant to
be -- 32 ms at 352800 -- rather than whatever the misalignment happened to leave.

A glitch every few seconds during development turned out not to be drift at all: it was a
diagnostic that scanned the whole ring inside the isochronous completion handler. Work on
the IO path costs a reboot to get wrong, so it is tempting to instrument heavily, but the
completion handler is a real time context and an expensive loop there is audible.

Three things had to be right for any of it to be audible, and each was silent when wrong:

**Zero timestamps land on exact multiples of the period.** A transfer carries `rate/250`
samples, which divides neither evenly nor into the period, so the boundary falls inside a
transfer and its host time is interpolated. Posting the transfer's own sample time instead
makes Core Audio log `TimeStampOutOfLine` continuously, reset its clock, and never advance
where it writes.

**The ring wraps at the zero timestamp period.** Not at the buffer length over the frame
width, which is the obvious reading and is wrong: the host writes and rewrites the first
period of the allocation while a driver using the whole allocation sweeps past it.

**The ring is read at the playback point, not the submission point.** What is submitted now
plays a whole in-flight window later, and the host writes barely ahead of what is playing, so
reading where the engine is queueing reads what the host has not written yet.

**The default format is the head of the published list, and it has to be asserted twice.**
Everything the OS plays goes out at whatever width the device defaults to, so building the
list in descriptor order handed the whole machine the narrowest format the DAC accepts -- this
one lists 16 bit first. `BuildFormats` leads with the widest subslot instead, which is also
what an application that takes the first format it is offered gets. Setting it on the stream
before `AddObject` is not enough on its own: it comes back as the narrowest published once
Core Audio picks the device up, so it is set again afterwards and both values logged.

## What is left: clicks during steady playback

Ordinary system audio -- YouTube, Spotify, `afplay` -- works, and the stutter that used to open
every fresh device start is gone. What remains is intermittent and smaller, and it is a
different fault from the one that produced the stutter.

**The symptom.** On some sessions the read point moves during steady playback, seconds after
the start. Each move is a jump in the audio: a click, and in a rising sweep the pitch visibly
steps, by roughly a fourth. Six of them in one session, spaced evenly, matched the rate limit
on corrections exactly -- so the limiter was metering the clicks out rather than preventing
them, and the underlying cause is that the engine drifts in front of the host during playback
rather than staying a fixed distance behind it.

**What is already ruled out.** The engine's own pacing is correct: measured against wall clock
it advances 109,058 frames in 579 ms at 192000, which is real time to within two parts in a
thousand. The zero timestamps are accurate too -- 32,768 frames in 4,095,991 mach ticks against
4,096,000 exact. Isochronous completions come back clean, `status 0` and a full 144 byte first
frame, at a steady 768 frames each. So neither the transfer chain nor the timeline arithmetic
is drifting.

**Where to look next.** Two candidates, both measurable without guessing:

* The feedback servo. `samples_per_microframe` comes from the DAC's feedback endpoint and is
  accepted anywhere within five percent of nominal, so a persistently high report would have
  the engine draining the ring faster than the host fills it. Log the accepted value against
  nominal over a session and see whether it sits off centre. Note that no feedback report has
  ever been observed in the log despite the pipe opening, which is itself unexplained -- the
  `DAC asks for` line has never appeared, and neither have the misses it would otherwise
  report.
* Core Audio's own convergence. Its counter follows the timeline the driver posts rather than
  leading it, and it closes a gap at a few thousand frames a second rather than instantly. A
  session that starts with the two far apart spends a long time converging, and the read point
  is chasing a moving target throughout.

**How to measure it.** `session ends: N read point moves, M cycles the engine had overtaken the
host, K frames sent as silence` is logged at every `StopIsoc`. `N` is the number of audible
clicks -- it has matched what a listener reports every time it has been checked, and it is the
number to drive to zero. `K` is how long the opening ramp lasted.

**A test signal helps.** A slow rising sine sweep makes both faults obvious where music does
not: a repeat is heard as the pitch dropping back, and a read point move as the pitch stepping.
Twenty seconds from 300 Hz to 1200 Hz at low amplitude is enough.

**Watch for the fail-silent trap.** An earlier version waited for the host to be pacing before
it would anchor, and sent silence until then. On sessions where that condition never arrived it
sent silence from end to end -- no audio at all, which is worse than the artefact it was
avoiding. Anchoring now falls back to a deadline. Any gate on the read path needs one.

## Iterating on this

`activate` alone does not swap the running code, so every change costs a round trip: rebuild,
`activate`, then `sudo pkill -f "SystemExtensions.*DsdAudioDriver"` and replug the DAC.

**Check the reload took before trusting a result.** A whole round of listening tests once ran
against a build that was never loaded, because the DAC came back before `activate` completed.
Mach-O links embed a fresh UUID, so compare the loaded binary against the copy inside the app
bundle -- the one that gets staged -- and not against `build/…dext/…`:

```
LOADED=$(ps aux | grep -i dsdaudio | grep -v grep | head -1 |
         grep -o '/Library/SystemExtensions/[^ ]*DsdAudioDriver')
shasum -a 256 "$LOADED" \
  build/DsdDriverInstaller.app/Contents/Library/SystemExtensions/\
com.github.xenide.dsdrust.driver.dext/Contents/MacOS/DsdAudioDriver
```

If `activate` fails with `OSSystemExtensionError 4`, a previous copy is pinned:
`systemextensionsctl list` will show one entry `activated enabled` beside another `terminating
for upgrade via delegate`. The `pkill` clears it, and the activate then succeeds.

**os_log drops lines from the IO path.** Counters that are summarised once per session are
trustworthy; a log line emitted per cycle is not, and reading a dropped line as an absent event
sent this work down a wrong path more than once.

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

**`activate` alone does not swap the running code.** A dext with a live provider keeps its
server process, and that process holds the bundle it launched from, so a rebuild stages and
enables and then keeps serving the old code: `systemextensionsctl list` shows the new copy
`activated enabled` beside the old one `terminating for upgrade via delegate`, and nothing
changes until the old server exits. Either `sudo pkill -f "SystemExtensions.*DsdAudioDriver"`
or unplug and replug the DAC. The same thing pins a dext whose `Start` returned an error,
where the next `activate` then tries to launch a bundle that is being deleted.

**Native DSD looks exactly like good PCM.** Core Audio has no DSD format, so native goes out
as big endian non-mixable integer PCM. Anything hunting for a bit-perfect format will choose
it and write PCM into it, and the DAC renders that as ticks at twice the nominal rate. Two
things keep that from happening by accident: PCM formats come first in the list, and they are
non-mixable too, so a player looking for a non-mixable format finds one that is not DSD. The
only thing separating the two is the big endian flag, which is what `StartDevice` reads to
choose an alternate setting.
