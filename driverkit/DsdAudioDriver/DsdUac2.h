//
//  DsdUac2.h
//  Reading a UAC2 configuration descriptor, without DriverKit.
//
//  This is the same knowledge src/output/usb/descriptors.rs carries, in the form the dext
//  needs it: not just the one RAW_DATA alternate setting, but every streaming alternate
//  setting on the interface, because a driver that owns the interface publishes all of them
//  as Core Audio formats and picks one per stream format change.
//
//  Nothing here includes a DriverKit header, so the parser builds and runs on the host under
//  driverkit/tests.
//

#ifndef DsdUac2_h
#define DsdUac2_h

#include <stddef.h>
#include <stdint.h>

namespace dsd {

/// Alternate settings one streaming interface may carry. UAC2 devices publish a handful.
constexpr size_t kMaxAltSettings = 16;
/// Discrete rates or subranges a clock source may report.
constexpr size_t kMaxSampleRates = 32;

/// Native DSD packs 32 DSD bits per channel per frame, so its subslot is four bytes wide.
constexpr uint8_t kNativeSubslotBytes = 4;

/// Widest sample frame the driver will publish a format for: two channels of four byte
/// subslots.
///
/// The ring is one allocation of a fixed stride, and the read path bounds itself on the ring
/// length in frames rather than on the mapping's length in bytes. A format wider than the
/// stride the ring was sized for therefore reads off the end of the mapping, on the IO path,
/// where a fault takes the machine down rather than the driver. Nothing narrows the channel
/// count on the way here -- it is whatever byte the descriptor carried -- so the bound has to
/// be applied before such a format is ever published.
constexpr uint32_t kMaxFrameBytes = 8;

/// Bytes one sample frame of this alternate setting occupies on the wire.
constexpr uint32_t AltFrameBytes(uint8_t channels, uint8_t subslot_bytes) {
    return static_cast<uint32_t>(channels) * static_cast<uint32_t>(subslot_bytes);
}

/// One streaming alternate setting, as the descriptor describes it.
struct AltSetting {
    uint8_t alt_setting;
    /// Bytes one channel occupies in a frame. Four for native DSD and for 32-bit PCM.
    uint8_t subslot_bytes;
    uint8_t bit_resolution;
    uint8_t channels;
    /// `bmFormats` bit 31: the subslots carry opaque bytes rather than PCM samples.
    bool raw_data;
    uint8_t out_endpoint;
    /// Zero when the endpoint runs without explicit feedback.
    uint8_t feedback_endpoint;
    /// The feedback endpoint's own service interval and payload, which are not the output
    /// endpoint's. A report arrives every 2^(interval - 1) microframes, so how much bus time
    /// a frame list submitted against it spans follows from this and nothing else.
    uint8_t feedback_interval;
    uint32_t feedback_max_packet;
    /// Payload bytes one microframe can carry, with the high-speed transaction multiplier
    /// already applied.
    uint32_t max_packet;
    uint8_t interval;
};

/// What the dext needs to drive one DAC's streaming interface.
struct Uac2Layout {
    uint8_t control_interface;
    uint8_t streaming_interface;
    uint8_t clock_id;
    AltSetting alts[kMaxAltSettings];
    size_t alt_count;
};

/// One publishable Core Audio format: a sample rate paired with the alternate setting that
/// carries it.
struct FormatEntry {
    double sample_rate;
    uint8_t alt_setting;
    uint8_t channels;
    /// Bits of the subslot that carry data. Below `subslot_bytes * 8` the samples are
    /// aligned high, which is what UAC2 24-in-32 means.
    uint8_t bit_resolution;
    uint8_t subslot_bytes;
    bool native_dsd;
};

/// Parse a configuration descriptor into the layout of its first audio streaming interface.
///
/// Returns false when the blob is malformed, carries no audio streaming interface, or has no
/// clock source to set a rate on -- in every case there is nothing to drive.
bool ParseLayout(const uint8_t* config, size_t length, Uac2Layout* out);

/// Whether `alt` can carry `rate` within one microframe.
bool AltCarriesRate(const AltSetting& alt, uint32_t rate);

/// Every (rate, alternate setting) pair the interface can carry, PCM first and native DSD
/// last: native is indistinguishable from non-mixable integer PCM through Core Audio, so
/// leading with it hands a DSD stream to applications that will fill it with PCM.
///
/// Returns the number written, which is capped at `capacity`.
size_t BuildFormats(const Uac2Layout& layout, const uint32_t* rates, size_t rate_count,
                    FormatEntry* out, size_t capacity);

/// Read a UAC2 clock `RANGE` report into a flat list of rates. A subrange whose resolution
/// is zero names a single rate; one with a resolution walks from min to max in steps.
///
/// Returns the number written, capped at `capacity`.
size_t ParseClockRange(const uint8_t* report, size_t length, uint32_t* out, size_t capacity);

}  // namespace dsd

#endif /* DsdUac2_h */
