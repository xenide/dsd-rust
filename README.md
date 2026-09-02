# dsd-rust

A bit-perfect command line DSD player for macOS. DSD bits reach the DAC exactly as they
are stored in the file, wrapped in DSD over PCM (DoP) 1.1.

## Usage

```
dsd-rust devices                       # output devices, and the DSD rates they accept
dsd-rust devices --formats             # plus every stream format each device advertises
dsd-rust info track.dsf                # container, rate, channels, duration
dsd-rust play track.dsf                # play on the default output device
dsd-rust play *.dsf --device "D50"     # pick a device by name fragment or UID
dsd-rust tui ~/Music/dsd               # browse, play, and watch the transport
```

`play` options: `--shared` leaves the device available to other apps, `--buffer-ms`
sizes the queue between the reader and the audio callback (default 500), and
`--buffer-frames` overrides the device IO buffer size. Both `--shared` and
`--buffer-frames` are Core Audio settings: a file that has to go over native DSD is
refused under `--shared`, because that path claims the DAC outright.

Files play in the order given, and the device is resolved once for the whole list: holding a
device exclusively moves the system default output elsewhere, so re-resolving between tracks
would pick the wrong one. Tracks may mix DSD rates; the device is reconfigured for each.

## Terminal UI

`dsd-rust tui [dir]` opens a file browser over `dir` (the working directory by default),
showing folders and DSD files only. It takes the same `--device`, `--shared`, `--buffer-ms`,
and `--buffer-frames` options as `play`.

```
 ↑↓ / j k     move             enter / → / l   open folder or play file
 ← / h        parent folder    space           play/pause
 s            stop             n / p           next / previous track
 r            re-read folder   q / esc         quit
```

Playing a file queues the whole folder from that file on, in the order the pane lists it.
Pausing keeps the DAC fed with DoP silence rather than stopping the stream, so the DAC holds
DSD lock and resuming costs no relock. The debug pane shows what the device settled on and
what the transport is doing right now:

```
device     Topping D50  exclusive, mixing off
transport  integer 24 bit, DoP 352800 Hz
io buffer  512 frames (1.5 ms)
queue       50%   44100 of 88200 frames
underruns  0 frames (0 ms of DoP silence)
frames     10584000 of 42336000
dsd        5644800 bit/s per channel, 84672000 bytes per channel
```

A rising `underruns` count means the reader is not keeping up; raise `--buffer-ms`. A `queue`
that sits near 0% is the same warning before it becomes audible.

## Native DSD

Some DACs top out at a PCM rate too low to carry the file as DoP. DoP packs 16 DSD bits into
each 24-bit frame, so DSD256 needs a 705600 Hz carrier; a DAC whose PCM ceiling is 384000 Hz
cannot reach it however willing the DAC is. Those DACs usually can still play the rate, over
an alternate setting Core Audio never offers because its format is `RAW_DATA` rather than PCM.

When the chosen device advertises no PCM rate able to carry a file, `play` takes that path
instead. It packs 32 DSD bits per channel into each USB frame with no markers and no carrier,
and paces the stream from the DAC's own feedback endpoint.

Reaching it means taking the device away from macOS. USB audio runs in a userspace daemon,
`usbaudiod`, which holds the audio interfaces and will not give them up. It re-acquires them
after a device enumerates, though, so `play` re-enumerates the DAC and claims the interfaces
in the window before the daemon does, then holds them for the session. No kernel extension,
no system extension, and no change to System Integrity Protection.

`devices` reports both paths, so the difference is visible before playing anything:

```
* Cayin RU7 Playback
    dop       176400/DSD64 352800/DSD128
    native    88200/DSD64 96000/DSD64 176400/DSD128 192000/DSD128 352800/DSD256 384000/DSD256
```

The native rates come from the DAC's own clock range report rather than from what the
endpoint could carry, so they are rates it will actually accept.

The cost is that claiming the DAC drops it off the USB bus for a moment and takes it away
from every other application until playback ends, which is heavier than Core Audio's hog
mode. While a native track is playing the DAC belongs to `dsd-rust` alone, so it does not
appear in `devices` and no other application can open it. Playback hands it back the same way it took it, by re-enumerating: releasing the
interfaces alone is not enough, because `usbaudiod` only looks at a device as it enumerates,
so a device it lost would stay missing until physically replugged. Ctrl-C hands it back too.

The DAC that plays natively is the one the target resolves to, matched by name against the
USB product string: Core Audio and USB name the same device differently -- "Cayin RU7
Playback" against "Cayin RU7" -- so whichever name contains the other counts as a match. A
DAC whose two names have nothing in common needs its USB name passed to `--device`, which
`devices` lists on the `native` line.

## What "bit-perfect" means here

* DSD samples are never resampled, filtered, or attenuated. The player only reorders bits
  and adds DoP marker bytes.
* DSF stores DSD least-significant-bit first; those bytes are flipped to MSB-first because
  that is the order DoP defines. DSDIFF is already MSB-first and passes through untouched.
* Each 24-bit PCM frame carries an alternating `0x05`/`0xFA` marker and 16 DSD bits, so the
  DAC recognises the stream as DSD and bypasses its PCM path.
* The player claims the device exclusively (hog mode) and prefers a stream format the device
  advertises as non-mixable, which takes Core Audio's mixer out of the path and hands the
  render callback the device's own integer samples. `play` reports the transport it got:
  `integer` when that works, `float32` when the device only offers mixable formats. Float32
  still carries every 24-bit code exactly, as long as nothing applies gain, and the player
  warns about a sub-unity device volume in that case only.
* Because the callback is handed whatever the stream's virtual format happens to be, the
  player waits for that format to settle before creating the callback. Sampling it too early
  would mean writing float samples into an integer buffer, which is noise, not a subtle fault.
* Underruns and the end of a track emit DSD silence (`0x69`), never PCM zero, so the DAC
  stays locked and does not pop.

The device format, sample rate, mixing switch, and hog mode are all restored when playback
ends, along with the system output device: claiming a device exclusively makes macOS pick a
different default, and it does not put it back on its own.

The same teardown runs on `Ctrl-C`, on `SIGTERM`, and when the terminal goes away, so an
interrupted player never leaves a DAC claimed. A second `Ctrl-C` skips the closing silence
and exits at once, still handing the device back.

## Supported files

* DSF (`.dsf`), DSD64 through DSD512, up to 6 channels
* DSDIFF (`.dff`), uncompressed `DSD ` sound data

DST-compressed DSDIFF and SACD ISO images are not supported.

## Requirements

macOS, a DAC that accepts DoP or native DSD, and a stable Rust toolchain. Build with
`cargo build --release`.
