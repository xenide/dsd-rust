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
host asked for 96000 Hz PCM, alternate setting 2, read lag 3072 frames
timestamp period for 96000 Hz: 32768 frames, 341 ms
host wrote 256 frames at sample 6324044 (ring slot 32588); engine reads at sample 6320697
                                        (slot 29241), so the host leads it by 3347
```

A cold open at the DAC's fastest rate, ten seconds after the engine retired, from a run that
switched rate under a playing video from 44100 up to 384000 and back down again:

```
host asked for 384000 Hz PCM, alternate setting 2, read lag 12288 frames
engine was down 56 periods of the DAC's clock: anchored at 45613056 (host time 511487899603),
                                               last pair was 38273024 (host time 511025338549)
streaming 384000 Hz on alt 2: ring 131072 frames of 6 bytes, timeline resumes at 45744128,
                              feedback endpoint 0x81 pipe open, interval 4 payload 4, 4 entries
first IO: op 1, 128 frames at sample 45752132; engine queues at 45751808, reads at 45739520,
                                               so the host leads the read point by 12612
client stops: 0 cycles the engine had overtaken the host, 0 cycles the host had lapped the
              read point, 1536 frames sent as silence
```

The host opens 12612 frames ahead of the read point against a read lag of 12288, which is the
geometry holding to 324 frames at the rate that used to fill the ring with the read lag alone.
Every rate in that run -- 44100, 48000, 96000, 176400, 192000, 352800, 384000, switched under
a playing video in both directions -- ran 0 crossings and 0 laps.

Native DSD is what the alternate setting exists for, and it is what Core Audio cannot reach
on its own. `dsd-rust play` takes it whenever the file's rate has no PCM carrier: DSD256
needs a 705600 Hz DoP carrier the DAC does not offer, and goes down alternate setting 4 at
352800 frames a second instead.

**The timestamp period is a duration, not a frame count.** The ring is exactly one period
long, and everything that has to fit inside it is measured in time: the read lag is 32 ms and
the host's buffer is one of its own cycles. A period fixed at 16384 frames is therefore a
ring of 371 ms at 44100 and 43 ms at 384000, where the read lag of 12288 plus a 4096 frame
host buffer fills it exactly and the host overwrites the slot the engine is about to read.
So the period is the smallest power of two covering 340 ms of the rate in force -- 16384 at
44100, 32768 at 96000, 65536 at 192000, 131072 at 352800 and 384000 -- which gives every rate
what 44100 already had.

It is bounded at both ends. Below, a timestamp lands on an exact multiple of the period
interpolated between two isochronous completions, so a period covering few transfers inherits
the jitter of individual ones: at 4096 frames and 352800 that was three transfers, and Core
Audio read a rate 7% out and spent a minute and a half walking back from it. Above, the period
is how often Core Audio hears what the clock is doing, and at 743 ms the host limped at a tenth
of rate for a second and a half before finding its feet.

`SetZeroTimeStampPeriod` is only legal inside a configuration change, so
`PerformDeviceConfigurationChange` is its only caller, and everything that needs the period
reads it back with `GetZeroTimestampPeriod` rather than deriving it from the rate again. The
host and the driver wrapping the ring at different lengths is the one thing that must never
happen. The allocation is sized once for the longest period at the widest frame, 1 MiB.

**What a period change does to the lattice, and the assumption in it.** Timestamps land on
multiples of the period counted from zero, and so does the ring's wrap: the driver reads at
`sample % period` and the host writes at the slot the same arithmetic gives, which is how the
two agree without ever exchanging a position. A period change relocates both lattices at once,
and the pair the host is holding -- a multiple of the old period -- is no longer a multiple of
the new one. That pair stays a true statement about the clock, which is all it is used for,
and the first pair posted after the change lands on the new lattice, leaving one interval
whose sample delta is not a whole period. If the host instead wraps relative to the pair it
holds, this is where it would show, and it shows immediately: the first write after a switch
into 352800 would land nowhere near the read point, and the `first IO` line says by how many
frames.

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

## This is a source build, not a release

There is no signed, notarised download and no plan for one. The driver names one DAC by
`idVendor` and `idProduct` in both its `Info.plist` and its entitlements, and pointing it at
another means editing those and rebuilding -- which a signed binary could not survive, so a
release would serve exactly one device and no one else. Both install paths below build from
this tree. "Point it at your DAC" is the supported way to use it on anything else.

That choice is what makes the format width bound below load bearing rather than defensive: the
DACs this gets pointed at are not this one, and a multichannel alternate setting is ordinary on
an interface.

## Where the ring is read

The read point is `sample_counter` minus two in-flight windows, fixed for the life of a
stream. It is derived, not measured, and that is the whole design: nothing about it depends on
where the host happens to be writing, so it cannot move, and a move is what a click was.

**Why that position.** The timeline maps a sample index to the time that index goes out on the
wire -- the pairs posted are a transfer's `start_sample` against the bus timestamp of its first
microframe. So the transfer starting at sample S must carry the host's audio for index S minus
the lag, and what is heard is the lag behind the timeline's own time for that index. That is
exactly the reported latency, and it is now a constant the driver can state.

**Why two windows and not one.** One is the read-ahead a transfer needs: the ring is read at
submission and a transfer spans a quarter of a window, so reading level with the timeline runs
its tail past the write head. The second is margin, and it has to come from here because it
does not come from the safety offset. Setting `SetOutputSafetyOffset` to a window does not make
Core Audio write a window ahead of the timeline -- measured against Chrome, it writes between
130 and 660 frames ahead whatever the offset says. Reading level with the timeline left under
one transfer of headroom, and the tail of every transfer came out as silence, which is heard as
noise rather than as a dropout.

**What it looks like settled.** At 352800 with 512 frame host buffers the host's write head
sits 8954 to 10507 frames ahead of the read point on a ring of 16384: clear of zero at one end
and of a lap at the other, with no trend.

**The two ways it could still be wrong are counted, not corrected.** `crossings` is the host
falling back onto the read point; `laps` is the host getting a whole ring ahead of it, so the
slot about to be read has been overwritten. The second is the nastier one -- what comes out is
continuous audio from further along the track, so it sounds perfectly fine and is heard only as
the sound running ahead of the picture. Both are impossible if the geometry holds, so a
non-zero count is a bug to find rather than a condition to correct.

**What this replaced.** The read point used to be anchored to the host's own write position and
corrected when it drifted. Every correction was a jump in the audio, and the position it landed
on depended on when in Core Audio's opening ramp the anchor was taken, which made the real
latency vary from session to session while the reported figure stayed constant. The margin was
`read_lag + 4 x in_frame_size`, which is 2048 frames against a 512 frame host buffer and 16384
-- the entire ring -- against the 4096 a browser asks for. That put the read point more than a
lap behind and every transfer picked up a slot already overwritten twice.

**The bound stays.** During Core Audio's opening ramp the audio does not exist yet, whatever
the read position: it writes at a fraction of real time for about a second after IO first
starts. The read is bounded by the host's own last write, with silence past it, and `starved`
counts how long that lasted.

**The host leaves holes in the ring, and they are filled rather than counted.** Core Audio's
IO cycle steps forward when a client misses its deadline: the sample time it hands the driver
jumps past frames it never wrote. They sit *behind* the write head, which is where the
starvation bound does not look, so the engine reads them, and what is in those slots is a ring
wrap old -- a full amplitude discontinuity at each end of the hole.

Nothing counted here can see it. The sample numbers are all correct, the margin is steady, and
`crossings` and `laps` stay at zero while the contents are wrong. What found it was the write
head's own arithmetic: consecutive cycles should advance by exactly one host buffer, and these
did not.

```
09:56:19.348  host jumped +720 frames beyond its 128-frame cycles
09:56:20.350  host jumped +572
09:56:22.017  host jumped +320
```

Measured at 384000 against two clients on the same machine, same rate, same cold open:

| client | host buffer | cycles skipped over | |
| --- | --- | --- | --- |
| Chrome | 128 frames, 0.33 ms | 32 in 42 s | about one a second |
| Spotify | 512 frames, 1.33 ms | 2 in 79 s | about one in forty |

128 frames is Web Audio's render quantum, which Chrome asks for whatever the rate, so at
384000 its audio thread has a third of a millisecond to answer and misses about once a second.
At 44100 the same buffer is 2.9 ms and it never misses. So the clicks were rate-dependent
without anything about the rate being wrong.

The driver patches the hole as soon as the cycle that skipped it reports, which is a read lag
before the engine reaches it, and `holes the host skipped over` counts them.

**Silence is not the patch, and measuring that is what took the second attempt.** A hole filled
with zeroes is two steps from full amplitude to nothing and back -- the same discontinuity the
stale audio had, at the same amplitude -- and a 0.67 ms one is heard exactly as loudly. The
counter said the fill was working, 32 holes in 42 seconds, while the clicks carried on at the
same rate. What goes there instead is a straight line, per channel, from the frame before the
hole to the frame after: both ends are frames the host did write, so the patch meets the audio
either side of it at its own value and there is no step at all. A millisecond of interpolation
once a second is a dulling, not a click.

Bridged up to five milliseconds; past that a straight line no longer resembles what it
replaces, and a hole that long is a client restarting rather than one running late, so it is
silenced. A raw carrier is always silenced: DSD is one bit per sample and the arithmetic
between two of them means nothing.

**A test signal helps.** A slow rising sine sweep makes these obvious where music does not: a
repeat is heard as the pitch dropping back, a read point move as the pitch stepping. Twenty
seconds from 300 Hz to 1200 Hz at low amplitude is enough.

## The cold open

Opening a stream on an engine that was not already running cost about a third of a second of
real audio. It was heard as the audio starting while the video spins, and then the video
running fast to catch up. Under `usbaudiod` the same clip on the same DAC does the opposite --
the video starts and the audio arrives late -- so it was never simply the client's own
start-up.

**What it looked like.** Chrome's audio thread stalled once, shortly after the stream opened.
Traced cycle by cycle at 192000, twelve cycles arrive dead on real time and then one does not:

```
ramp cycle 12: host wrote 4096 frames at 6721556, margin 6419, silence so far 4608   (25.402)
ramp cycle 13: host wrote 8192 frames at 6787504, margin 3245, silence so far 63218  (25.760)
```

358 ms with no write, and the host comes back 65948 frames -- 343 ms -- further along the
timeline. The engine free-runs through the gap and sends 58610 frames of silence. That one stall
is the whole session's starvation: 4608 before it, 63218 after, nothing afterwards. The audio
clock has advanced through a third of a second that never contained audio, and the video
pipeline follows that clock.

**What it was not.** The ring geometry is not involved: the session ran 0 crossings and 0 laps
with the margin steady between 5908 and 7853. Pacing is not involved either -- cycles 1 through
12 are exactly one 4096 frame buffer every 21.5 ms. And buffering could not have absorbed it.
The cushion against a stall is `read_lag` and nothing else: starvation begins `margin / rate`
after the host stops, which is 6400/192000, about 33 ms. Riding out 358 ms needs 358 ms of
`read_lag` -- and `read_lag` is what `SetOutputLatency` reports, so that is 358 ms of output
latency, far more than `usbaudiod` reports at this rate. It is not reachable in any case: the
ring is one zero timestamp period because the host wraps there whatever the buffer's length,
and the period cannot grow to suit 192000 without breaking 44100, where 32768 was already
measured making the host limp at a tenth of rate for a second and a half. Growing the ring
alone moves the lap threshold and not the cushion.

**What it was: a timeline with a hole in it.** `StopIsoc` leaves the last pair the engine
posted as the newest thing the host holds, and `StartIsoc` used to resume one period on from
it. That put the engine where the host writes, which is right, and it also handed Core Audio an
interval of one period spanning however long the engine had been down. Read as a clock, that is
a device running at a tiny fraction of rate -- 16384 frames in five minutes -- and a host
scheduling its next cycle from it sleeps far too long. It sleeps once, wakes, sees how far
behind it is, and writes a double buffer to catch up. That is the whole signature: one stall,
then a session with nothing else wrong with it. It is also the fault the engine outliving its
client was working around from the other end, where the same thing reads as "about a second in
which Core Audio has not found its rate".

**So the timeline runs on through the gap.** The DAC's crystal never stopped; only the driver's
counter did. `StartIsoc` works out where that clock would have reached -- elapsed host time
since the last pair, at the rate about to be streamed -- and resumes on the period boundary
nearest it, posting that boundary against the host time the clock passes it, before IO starts.
Core Audio then reads one continuous timeline at one rate across the gap, and has nothing to
walk back from. What the boundary quantisation costs is spread over the whole gap: half a
period of samples against minutes of it.

The anchor is posted exactly one period before the first transfer goes out, which is what stops
where the host opens depending on the client. Whether the host projects to the boundary after
the newest pair or extrapolates forward from its host time, both land on the sample the engine
starts at. Fitting a constant offset is what could not be made to do that: one period on suited
Chrome, which opened 23048 frames ahead, and put afplay 21232 frames the other way.

Turning the gap into sample frames needs the host clock's tick rate, so the engine measures it
over the length of each stream and keeps it. Nothing in a stream needs it; by the time a start
does, there is no stream left to measure it from. Before the first one there is no measurement
and no pair, and the resume falls back to a period on from whatever `GetCurrentZeroTimestamp`
reports, which is zero.

**Measured, on the same DAC at 192000 with the teardown at ten seconds.** Two cold opens, one
after 21 seconds down and one after 10:

```
engine was down 246 periods of the DAC's clock: anchored at 9093120 (host time 154754874456),
                                                last pair was 5062656 (host time 154251275208)
client stops: 0 cycles the engine had overtaken the host, 0 cycles the host had lapped the
              read point, 4608 frames sent as silence
```

4608 frames is the pre-roll before the host's first write, and it is the cheapest open in the
log, warm ones included. The same binary before the change, at the same teardown, cost 63218,
148228 and 181518 for its three cold opens against 4608 to 15361 for the warm ones, and took
770 ms to reach the host's first write where this takes 36.

That first line is also the arithmetic: 4030464 frames of timeline over 503599248 host ticks is
23.99 MHz, which is the mach timebase. Posting a period across that same gap instead claimed
780 Hz on a device running at 192000, and sleeping off a rate 246 times slow is what the stall
was. The second open shows what the quantisation costs: 0.4% of rate over a 10.5 second gap,
which is half a period spread across the whole of it.

`engine was down N periods of the DAC's clock` says the resume ran; `resuming a period on from`
says it fell back.

**The short gap takes the same reasoning without the anchor.** A format change puts `StopIsoc`
and `StartIsoc` about twenty milliseconds apart, which is less than a period, and an anchor a
period back from the first transfer would be dated before the pair the host is already holding.
So nothing is posted. The timeline still runs on to where the clock has reached, and the host
keeps the pair it has -- under a period old, which is as fresh as the anchor would have made
it, and describing this timeline exactly: the counter resumed here is what the host projects
from that pair.

Landing between two boundaries rather than on one has two consequences in the code.
`next_timestamp_at` is the first boundary above the resume rather than a period past it, and
the first completion seeds a pair only where the sample it starts at is a boundary. A zero
timestamp names the sample the host wraps the ring at, so one posted off the lattice moves
where the host writes.

**Skipping to the next boundary instead is what this replaced, and it was bounding the period
from above.** It handed Core Audio one period of samples across the twenty millisecond gap, a
rate of `period / gap` -- four times too fast at 16384 frames, for the eighty-five milliseconds
until the boundary landed. That is small enough to pass a listening test at 16384, and it is
why scaling the period with the rate failed outright: at 65536 for 192000 and 131072 for
352800, the same lie is eighteen times too fast and 371 ms long. Measured mid-song at 192000,
the host wrote 224 frames a cycle while its sample time advanced 64000 every 27 ms, about
twelve times real time, until it caught up and snapped back to a steady margin. Heard as noise,
and as half a second repeating where the write head lapped the read point on the way past, with
laps in the tens of thousands where the fixed period had tens.

**Sample zero is a real sample, and testing for it cost 695 laps.** "No pair yet" was read off
`GetCurrentZeroTimestamp` as a sample of zero, which is also where the first stream of a driver
load starts its timeline and posts its first pair. A client that changed rate two hundred
milliseconds into that first stream therefore resumed from zero a second time, onto a timeline
Core Audio had already left:

```
streaming 352800 Hz on alt 2: ring 131072 frames of 6 bytes, timeline resumes at 0
seeded the timeline at sample 0, host time 506713158666
first IO: op 1, 512 frames at sample 140872; engine queues at 7056, reads at 0,
                                             so the host leads the read point by 140872
```

The host was 140872 frames along and the engine was at zero. Core Audio walked the difference
off over the next 1.4 seconds -- 203442, 191729, 20545, 15888 -- lapping the read point 695
times on the way, which is the sandy, clicking noise the section above describes for two
timelines anchored differently. What says there is no pair is the pair's *host time*, which no
real pair has as zero.

**One thing tried that is not it, and is worth not trying again.** Anchoring the read point to
the host's first write shifts the read point without shifting `SetOutputLatency`, which makes
the true latency `read_lag` minus the shift and therefore negative -- audio ahead of the
timeline, which is the fault it was meant to remove.

**A warm open never paid this**, which is what the engine outliving its client was for, and it
is also why the idle teardown was fifteen minutes for a while: the window had become a way of
avoiding a cold open rather than a judgement about what idle streaming is worth. It is back to
ten seconds now that the two cost the same.

## Geometry is published on a configuration change, not at IO start

The output latency and safety offset both scale with the rate -- the read lag is two in-flight
windows, and a window is a fixed number of microframes -- and the driver set them in
`StartDevice`. That is too late. A client reads them when it opens the device, which is before
IO starts, so a client that changed rate laid out its writes by the rate it had just left and
kept doing so for the life of its stream.

The pair that says so is half a second apart, same rate, same 512 frame host buffer, differing
only in what played before:

```
96000 after 352800   read lag 3072   margin 9896   382 laps
96000 after 96000    read lag 3072   margin 3740     0 laps
```

A margin wider than the ring less the host's buffer means the host overwrites the slot the
engine is about to read, so what comes out is audio from a period further on. It is not heard
as a glitch, which is why `laps` is counted: 384000 followed by 192000 ran at a margin of 13700
against a threshold of 12288 for minutes, through two engine teardowns, because nothing re-reads
the property once a stream is open.

`DsdAudioDevice` exists for this. It overrides `PerformDeviceConfigurationChange` and
`HandleChangeSampleRate` and republishes the geometry from there, which is where state that
affects IO is supposed to be published: the host has stopped IO, and it re-reads the device when
the call returns. `StartDevice` still calls the same `PublishGeometry`, as the backstop for a
start no configuration change preceded, and it is a no-op when the numbers have not moved.

**This is what sized the period against the rate.** The read lag at 384000 is 12288, so on a
ring of 16384 with a 4096 frame host buffer the lap threshold -- the ring less the buffer -- is
12288, which the read lag reaches before the host has written anything. 352800 cleared it by
about a thousand frames. A longer period was blocked twice over: by the short gap skipping to
a boundary, which scaled the rate error with the period, and by 32768 making the host limp for
a second and a half at 44100, a limp that was Core Audio failing to find its rate. The first is
fixed in "The cold open" above and measured; the second is what that section addresses. At
131072 the read lag is a tenth of the ring at both rates.

## The feedback endpoint, and three ways to lose a servo

An asynchronous endpoint runs on the DAC's clock rather than the host's and says, once per
service interval, how many samples it wants per microframe as a 16.16 fixed point count. This
one had never produced an observed report, and looked like an endpoint that was never asked:
the pipe opened, `SubmitFeedback` returned success, and nothing followed. No report, no miss,
no line at all.

It was being asked. Three faults stacked, and each turned off its own evidence.

**The frame list is one entry per service interval, not per microframe.** The RU7's feedback
endpoint has `bInterval` 4, so a report arrives every 2^3 = 8 microframes and 32 entries span
32 bus frames. The chain advanced its target by 4, which is an *output* transfer's span. The
first submission went out, its completion arrived 32 ms later, and everything after it was
refused `kIOReturnIsoTooOld` against a frame 28 ms gone. The parser had never read the feedback
endpoint's own `bInterval`: `ReadEndpoint` returned early for the IN endpoint after recording
its address, so `alt->interval` and `alt->max_packet` are the output endpoint's and always
were.

**A chain with one transfer outstanding has no lead of its own.** Even with the span right,
resubmitting from its own completion aims at the frame that transfer just finished, which has
gone by the time the handler runs. A private counter does not help -- it only advances on
success, so the first refusal pins it and every retry afterwards aims at the same receding
frame. It schedules against the output chain's queue point now, which is a whole in-flight
window into the future and always a frame the controller will still take.

**The transfer's aggregate status is not whether the reports arrived.** This DAC's feedback
transfers come back `kIOReturnOverrun` every time while all four intervals inside them are
marked success and hold their four bytes -- not some of them, all of them. The accept path
gated on the aggregate and discarded 475 completions in a row.

All of it was invisible, which is the part worth carrying forward. Reports were logged only
every 2000th, so the 32 that one surviving transfer collected never printed. Any report at all
suppressed the miss line. The re-arm was gated on `feedback_reports == 0`, so a chain that got
one completion and then died was permanently excluded from restart. A servo that ran for 32 ms
and froze on its last value looked identical to one that never started.

**What the DAC asks for.** 24.000732 samples per microframe against a nominal 24 at 192000,
steady to the last digit across a five minute session: the crystal runs about thirty parts per
million fast. Open loop that is roughly 1700 frames of the DAC's own buffer per five minute
track, which is what the servo exists to absorb.

**Five minutes at 192000 with the loop closed**, from the session that closed it. The counters
have been renamed since -- the read point is derived now, so nothing counts it moving -- but
the feedback line is unchanged:

```
session ends: 1 read point moves, 0 cycles the engine had overtaken the host,
              159748 frames sent as silence
feedback over the session: 6855 submits, 6854 completions, 27416 reports, 0 misses, 0 re-arms
```

| | full session | last three minutes |
| --- | --- | --- |
| drift | -0.3 ppm | +0.4 ppm |
| host write rate | 192007.30 Hz | 192006.43 Hz |

The DAC asks for 192005.86 Hz and Core Audio writes at 192006.43, so the host has followed the
driver's timeline onto the DAC's crystal to within three parts per million. The first fifty
seconds of that session read -11.8 ppm and the last three minutes read +0.4: the early number
is Core Audio converging onto the rate the servo stepped to, not a standing drift, and only a
run long enough to hold both tells them apart.

## The engine outlives the client

`StopDevice` used to tear the isochronous stream down, so every client paid a cold start:
about a second in which Core Audio has not found its rate, the engine has nothing real to
send, and the frames the host writes meanwhile are read by nobody. One client opening once
hides that. A browser does not -- it opens and closes the stream several times while a page
settles, and each open cost another second:

```
12:56:28 streaming -> ends 12:56:33   220422 frames silence
12:56:37 streaming -> ends 12:56:42   244998 frames silence
12:56:47 streaming -> ends 12:57:18   215813 frames silence
```

Watched, that is a YouTube tab whose audio starts while the picture sits on a spinner at
0:00, then races to catch up: the audio clock had run through several seconds nobody heard.
Spotify and VLC open once and hold, which is why they never showed it, and setting the device
to 48000 changed nothing, which is what ruled out resampling.

A running audio device keeps its clock running whether or not anything is playing. The engine
now does too. `StopDevice` marks the client inactive and leaves the stream going on silence,
still posting timestamps; `StartDevice` lets a client whose rate, alternate setting and frame
width already match straight in, with no `StartIsoc`, no re-anchor and no wait, because the
timeline and the ring geometry never stopped being valid. A format change restarts it, from
`StartDevice` rather than `StopDevice`, and unplugging still tears it down.

It does not outlive it indefinitely. The completion handler counts how long it has run with no
client -- a transfer spans a fixed number of microframes, so the count is a wall clock at every
rate -- and past ten seconds the chains stop resubmitting and the interface drops back to its
zero bandwidth alternate setting. Ten seconds is far longer than the gap between two tracks and
far shorter than the streaming is worth paying for: at native DSD the engine reserves about
3 MB/s in the periodic schedule and wakes the driver 500 times a second to send silence.

The teardown runs from a dispatch queue rather than from the completion that triggers it,
because `StopIsoc` aborts the pipe synchronously and then frees the buffers and the OSAction
that completion is running on.

One consequence remains deliberate: after a DSD track the DAC stays on that alternate setting
until the teardown, so its display keeps showing DSD for those ten seconds.

## Iterating on this

`activate` stages a build; it never swaps the running code. Deactivate first, every time:

```
# with the DAC unplugged
sudo pkill -f "SystemExtensions.*DsdAudioDriver"
./build.sh app
build/DsdDriverInstaller.app/Contents/MacOS/DsdDriverInstaller deactivate
systemextensionsctl list      # must show no dsdrust entry at all
build/DsdDriverInstaller.app/Contents/MacOS/DsdDriverInstaller activate
# then plug the DAC back in
```

Activating over a live entry is what costs the round trips. Two copies then sit on file,
`sysextd` keeps handing out the stale one, and staging a new copy strips the exec bit from the
outgoing one -- so the pinned server cannot even be relaunched, and the kernel gives up with
`failed to find server`. The DAC falls back to `usbaudiod` and the driver logs nothing at all,
which reads exactly like a driver that loaded and stayed quiet. Deactivating first avoids the
whole state; `find /Library/SystemExtensions -name DsdAudioDriver -exec ls -l {} \;` shows what
is really on file and which copies are still executable.

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

If `activate` fails with `OSSystemExtensionError 4`, two copies are on file and `sysextd` will
not choose between them: `log show --predicate 'process == "sysextd"'` says `activateDecision
found two entries`, one `activated_enabled` beside one `terminating_for_upgrade_via_delegate`.
The pinned one is still running because it still owns the DAC, so the kill has to come before
the activate rather than after it.

Once two entries are on file, killing and activating again does not resolve it -- `sysextd`
keeps handing out the stale one, and a stale copy has been seen staged without its exec bit,
which fails the launch outright rather than loading the wrong build:

```
launchd: access(/Library/SystemExtensions/<uuid>/…/DsdAudioDriver, X_OK) failed with errno 13
DK: DsdAudioDriver-0x… failed to launch server
```

The DAC then falls back to `usbaudiod` and the driver logs nothing at all, which reads exactly
like a driver that loaded and stayed quiet. `system_profiler SPAudioDataType` tells the two
apart: the device reports `Manufacturer: dsd-rust` under this driver and `Manufacturer: <the
DAC's own>` under `usbaudiod`.

Clearing it takes a deactivate, with the DAC unplugged so nothing pins anything:

```
sudo pkill -f "SystemExtensions.*DsdAudioDriver"
DsdDriverInstaller deactivate
systemextensionsctl list          # must show no dsdrust entry at all
DsdDriverInstaller activate
```

then plug the DAC back in. `find /Library/SystemExtensions -name DsdAudioDriver -exec ls -l {} \;`
shows what is really on file, which is one line per staged copy and their permissions.

**os_log drops lines from the IO path.** Counters that are summarised once per session are
trustworthy; a log line emitted per cycle is not, and reading a dropped line as an absent event
sent this work down a wrong path more than once.

## Layout

| file | what it is |
| --- | --- |
| `DsdAudioDriver/DsdUac2.{h,cpp}` | UAC2 descriptor parsing and the format list. No DriverKit. |
| `DsdAudioDriver/DsdAudioDriver.iig` | The driver class, as iig reads it. |
| `DsdAudioDriver/DsdAudioDriver.cpp` | Matching, the audio objects, and the isochronous engine. |
| `DsdAudioDriver/DsdAudioDevice.{iig,cpp}` | The device. Publishes geometry on a config change. |
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

**Alternate settings wider than eight bytes a frame are dropped, not published.** The ring is
one allocation of a fixed stride, so an interface offering more than two channels of four byte
subslots will appear with those settings missing rather than with a driver that reads off the
end of its mapping. `dsd::kMaxFrameBytes` is the bound, and it is the same constant the ring is
sized from. Raising it means raising the ring with it.

The dext bundle is named for its bundle identifier, `com.github.xenide.dsdrust.driver.dext`,
because the system copies it out of the app by that name. Renaming it breaks installation
with no useful message, and a bundle identifier over 63 characters fails the same way.

## Notes for whoever picks this up

**Volume and mute belong to the DAC.** The driver publishes no volume or mute control, so the
system volume keys do nothing for this device and applications see a device that cannot be
attenuated. That is deliberate. Any volume the host applies is arithmetic on the samples, which
is the one thing a bit-perfect path must not do, and native DSD has no meaningful software
volume at all -- the bits are a one bit stream whose amplitude is its density. The RU7 has a
hardware volume control, and that is where the level is set.

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

**One class per `.iig`.** The dispatch glue iig generates includes `<framework>/<ClassName>.h`
by name, so two classes in one def file fail to compile with a missing header rather than
anything that names the cause. `build.sh` runs iig once per class, device before driver.

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
