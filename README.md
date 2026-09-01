# dsd-rust

A bit-perfect command line DSD player for macOS. DSD bits reach the DAC exactly as they
are stored in the file, wrapped in DSD over PCM (DoP) 1.1.

## Usage

```
dsd-rust devices                       # output devices and the DoP rates they accept
dsd-rust devices --formats             # plus every stream format each device advertises
dsd-rust info track.dsf                # container, rate, channels, duration
dsd-rust play track.dsf                # play on the default output device
dsd-rust play *.dsf --device "D50"     # pick a device by name fragment or UID
```

`play` options: `--shared` leaves the device available to other apps, `--buffer-ms`
sizes the queue between the reader and the audio callback (default 250), and
`--buffer-frames` overrides the device IO buffer size.

Files play in the order given, and the device is resolved once for the whole list: holding a
device exclusively moves the system default output elsewhere, so re-resolving between tracks
would pick the wrong one. Tracks may mix DSD rates; the device is reconfigured for each.

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

macOS, a DAC that accepts DoP, and a stable Rust toolchain. Build with `cargo build --release`.
