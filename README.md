# dsd-rust

A bit-perfect command line player for macOS. Samples reach the DAC exactly as the file
stores them: DSD wrapped in DSD over PCM (DoP) 1.1 or sent natively, and PCM at the file's
own sample rate. Either way the device is claimed and locked to the rate the recording
wants, so nothing resamples, mixes, or attenuates on the way.

It reads DSF, DSDIFF, FLAC, and SACD disc images.

## Usage

```
dsd-rust devices                       # output devices, and the DSD rates they accept
dsd-rust devices --formats             # plus every stream format each device advertises
dsd-rust info track.dsf                # tags, container, rate, channels, duration
dsd-rust play track.dsf                # play on the default output device
dsd-rust play album.flac               # PCM, at the file's own rate and width
dsd-rust play disc.iso                 # every track of a SACD image, in order
dsd-rust play *.dsf --device "D50"     # pick a device by name fragment or UID
dsd-rust tui ~/Music                   # browse, play, and watch the transport
```

`play` options: `--shared` leaves the device available to other apps, `--buffer-ms`
sizes the queue between the reader and the audio callback (default 500), and
`--buffer-frames` overrides the device IO buffer size. Both `--shared` and
`--buffer-frames` are Core Audio settings: a file that has to go over native DSD is
refused under `--shared`, because that path claims the DAC outright.

Files play in the order given, and the device is resolved once for the whole list: holding a
device exclusively moves the system default output elsewhere, so re-resolving between tracks
would pick the wrong one. A playlist may mix DSD and PCM and mix rates within either; the
device is reconfigured for each track. A disc image joins the list as the tracks it holds.

## Tags

Every container can name the recording, and `info`, the file browser, and the transport all
show what they carry. DSF files keep an ID3v2 tag past the audio; DSDIFF files keep the
artist and title in an edited-master information chunk, which may sit either side of the
sound data; FLAC files keep a Vorbis comment; a SACD image names its album and every track
in the disc's own text blocks. Only the title, artist, album, and track number are read:
artwork and the rest of a tag are skipped. A file with no tags, or one whose tag will not
parse, still lists and plays under its filename.

```
dsd-rust info track.dsf
track.dsf
    title       So What
    artist      Miles Davis
    album       Kind of Blue
    track       1
    container   DSF
    format      DSD64 (2.8224 MHz), 2 ch
```

## Terminal UI

`dsd-rust tui [dir]` opens a file browser over `dir` (the working directory by default),
showing folders, disc images, and playable files only. A tagged file lists under its track
number and title, an untagged one under its filename, and either way the pane keeps the order
the files sort in on disk. A disc image opens like a folder, listing the tracks inside it in
the order the disc numbers them. It takes the same `--device`, `--shared`, `--buffer-ms`,
and `--buffer-frames` options as `play`.

```
 ↑↓ / j k     move             enter / → / l   open folder or play file
 ← / h        parent folder    space           play/pause
 s            stop             n / p           next / previous track
 , / .        back / on 5 s    q / esc         quit
 r            re-read folder
```

Playing a file queues the rest of the listing from that file on, in the order the pane shows
it -- the folder's other files, or the disc image's remaining tracks.
Pausing and seeking keep the DAC fed with DoP silence rather than stopping the stream, so the
DAC holds DSD lock and neither costs a relock. Seeking drops what is queued and restarts the
reader at the new position, so the jump takes about as long as it takes to refill. The debug
pane shows what the device settled on and what the transport is doing right now. Above it the
transport names the recording, artist and title with the album beneath, falling back to the
filename for a file that carries no tags:

```
device     Topping D50  exclusive, mixing off
transport  integer 24 bit, DoP 352800 Hz
io buffer  512 frames (1.5 ms)
queue       50%   44100 of 88200 frames
underruns  0 frames (0 ms of DSD silence)
frames     10584000 of 42336000
stream     5644800 bit/s per channel, 84672000 bytes per channel
```

A rising `underruns` count means the reader is not keeping up; raise `--buffer-ms`. A `queue`
that sits near 0% is the same warning before it becomes audible.

## PCM

A FLAC file is decoded and handed to the device at its own sample rate and width. The device
is claimed the same way a DSD file claims it -- hog mode, mixing off where the device offers
the switch, and the stream format set to the file's rate -- so macOS neither resamples the
file nor mixes anything else into it. A DAC that does not offer the file's rate is told so
rather than played to at a rate it does offer: resampling is the one thing this player will
not do.

Samples are left-justified into the same 24-bit word DoP frames use, which is a shift by a
power of two and so exact; a 16-bit file reaches the DAC as the codes it stores, scaled but
never rounded. FLAC wider than 24 bits is refused rather than truncated.

Seeking uses the file's seek table where it has one, so a jump costs a single read. A file
written without one is decoded forward to the target instead, which is why only a seek
backwards through such a file takes a moment.

## SACD disc images

A `.iso` rip of a SACD is a whole disc, so it opens as a list of tracks: `play disc.iso`
plays all of them in order, and the file browser descends into the image the way it descends
into a folder. The two-channel area is used where the disc has one, and the multichannel area
where it does not.

Almost every SACD stores its DSD compressed with DST, so an image is decoded rather than
copied: the arithmetic coder, the prediction filters and the probability tables are read from
each frame and reset at its start. DST is lossless, so what reaches the DAC is the DSD the
disc was mastered with, bit for bit. Decoding runs at about twenty times real time per core
for stereo DSD64, so it keeps ahead of playback with room to spare.

Because every frame stands on its own, a seek needs no history: the track's sectors are
halved by the timecode each audio frame carries, which finds the frame holding the target in
about twenty reads rather than by decoding the distance.

## Native DSD

Some DACs top out at a PCM rate too low to carry the file as DoP. DoP packs 16 DSD bits into
each 24-bit frame, so DSD256 needs a 705600 Hz carrier; a DAC whose PCM ceiling is 384000 Hz
cannot reach it however willing the DAC is. Those DACs usually can still play the rate, over
an alternate setting Core Audio never offers because its format is `RAW_DATA` rather than PCM.

When the chosen device advertises no PCM rate able to carry a file, `play` takes that path
instead. It packs 32 DSD bits per channel into each USB frame with no markers and no carrier,
and paces the stream from the DAC's own feedback endpoint.

Before it does, it checks whether the device is really out of carrier. Core Audio derives the
formats it advertises from the intersection of the streaming alternate settings with the clock
ranges, and that intersection is sometimes narrower than what the hardware takes. Where the
DAC's own clock range report claims a DoP carrier rate Core Audio does not advertise, `play`
sets that rate, reads back what stuck, and puts the device where it found it. A DAC that
answers yes plays over DoP and never touches the interface claim below. The probe runs once
per rate per session, and only for rates the two sources disagree on.

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

* Samples are never resampled, filtered, or attenuated. For DSD the player only reorders
  bits and adds DoP marker bytes; for PCM it only shifts codes into a wider container, which
  scales every one of them by the same power of two.
* A file is played at its own rate or not at all. The device is set to the rate the recording
  wants, and a device that will not take that rate is reported rather than worked around.
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
* Underruns and the end of a DSD track emit DSD silence (`0x69`), never PCM zero, so the DAC
  stays locked and does not pop. A PCM track emits zero, which is what silence is there.
* DST decoding is lossless by construction, and the decoder is exact: no filtering or
  dithering stands between a disc image and the DoP frames the DAC receives.

The device format, sample rate, mixing switch, and hog mode are all restored when playback
ends, along with the system output device: claiming a device exclusively makes macOS pick a
different default, and it does not put it back on its own.

The same teardown runs on `Ctrl-C`, on `SIGTERM`, and when the terminal goes away, so an
interrupted player never leaves a DAC claimed. A second `Ctrl-C` skips the closing silence
and exits at once, still handing the device back.

## Supported files

* DSF (`.dsf`), DSD64 through DSD512, up to 6 channels
* DSDIFF (`.dff`), uncompressed `DSD ` sound data
* FLAC (`.flac`), any sample rate the device offers, 16 or 24 bit, up to 8 channels
* SACD disc images (`.iso`), DSD64, DST-compressed or not

DST-compressed DSDIFF is not supported; the same compression inside a disc image is.

## Requirements

macOS, a DAC that accepts DoP or native DSD for DSD material (any output device will do for
PCM), and a stable Rust toolchain. Build with `cargo build --release`.
