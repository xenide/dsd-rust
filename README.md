# dsd-rust

A bit-perfect command line DSD player for macOS. DSD bits reach the DAC exactly as they
are stored in the file, wrapped in DSD over PCM (DoP) 1.1.

## Usage

```
dsd-rust devices                       # output devices and the DoP rates they accept
dsd-rust info track.dsf                # container, rate, channels, duration
dsd-rust play track.dsf                # play on the default output device
dsd-rust play *.dsf --device "D50"     # pick a device by name fragment or UID
```

`play` options: `--shared` leaves the device available to other apps, `--buffer-ms`
sizes the queue between the reader and the audio callback (default 250), and
`--buffer-frames` overrides the device IO buffer size.

## What "bit-perfect" means here

* DSD samples are never resampled, filtered, or attenuated. The player only reorders bits
  and adds DoP marker bytes.
* DSF stores DSD least-significant-bit first; those bytes are flipped to MSB-first because
  that is the order DoP defines. DSDIFF is already MSB-first and passes through untouched.
* Each 24-bit PCM frame carries an alternating `0x05`/`0xFA` marker and 16 DSD bits, so the
  DAC recognises the stream as DSD and bypasses its PCM path.
* The player claims the device exclusively (hog mode) and turns off Core Audio mixing where
  the device allows it, then asks for an integer stream format. `play` reports whether the
  transport ended up `integer` or `float32`; float32 still carries every 24-bit code exactly,
  as long as the device volume is at unity, and the player warns when it is not.
* Underruns and the end of a track emit DSD silence (`0x69`), never PCM zero, so the DAC
  stays locked and does not pop.

The device format, sample rate, mixing switch, and hog mode are all restored when playback
ends.

## Supported files

* DSF (`.dsf`), DSD64 through DSD512, up to 6 channels
* DSDIFF (`.dff`), uncompressed `DSD ` sound data

DST-compressed DSDIFF and SACD ISO images are not supported.

## Requirements

macOS, a DAC that accepts DoP, and a stable Rust toolchain. Build with `cargo build --release`.
