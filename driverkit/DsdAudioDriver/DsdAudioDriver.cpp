//
//  DsdAudioDriver.cpp
//

#include <DsdAudioDriver/DsdAudioDriver.h>

#include <AudioDriverKit/AudioDriverKit.h>
#include <DriverKit/DriverKit.h>
#include <DriverKit/IOBufferMemoryDescriptor.h>
#include <DriverKit/IODispatchQueue.h>
#include <DriverKit/IOLib.h>
#include <DriverKit/IOMemoryMap.h>
#include <DriverKit/OSSharedPtr.h>
#include <USBDriverKit/USBDriverKit.h>

#include "DsdUac2.h"

#define Log(fmt, ...) os_log(OS_LOG_DEFAULT, "DsdAudioDriver: " fmt, ##__VA_ARGS__)

namespace {

/// Microframes one transfer carries, so four milliseconds of audio.
constexpr uint32_t kMicroframesPerTransfer = 32;
/// Transfers in flight, giving sixteen milliseconds of scheduling headroom. Fewer than
/// this once cost the completion handler its margin; more buys nothing and costs latency,
/// because the read point trails the queue point by the whole in-flight window.
constexpr uint32_t kTransfersInFlight = 4;
/// High-speed USB divides each one millisecond bus frame into eight microframes.
constexpr uint64_t kMicroframesPerFrame = 8;
/// Bus frames to schedule the first transfer ahead of now.
constexpr uint64_t kStartLead = 10;

/// Milliseconds of bus time one transfer covers. A transfer spans a fixed number of
/// microframes rather than a fixed number of samples, so counting completions is a wall
/// clock at every rate the DAC runs.
constexpr uint64_t kTransferMs = kMicroframesPerTransfer / kMicroframesPerFrame;
/// How long the engine keeps streaming after its last client stopped.
///
/// The engine outliving its client is what makes the next one start warm, and a client
/// returning inside this window pays nothing for it -- a browser opening and closing the
/// stream while a page settles is well within it. Past it the streaming buys nothing: it
/// holds an alternate setting's bandwidth reserved in the periodic schedule and wakes the
/// driver every transfer to send silence no one is listening to. So the interface goes back
/// to its zero bandwidth setting, which is what that setting is for, and the client after
/// the window pays one relock instead.
constexpr uint64_t kIdleTeardownMs = 10000;
constexpr uint64_t kIdleTeardownCompletions = kIdleTeardownMs / kTransferMs;

/// Sample frames between the timestamps the host reads to build its timeline, which is also
/// the length of the ring the two share.
///
/// Sized for the fastest rate any DAC here publishes, not for the slowest, because both
/// things that depend on it get worse as the rate rises.
///
/// The host writes a safety offset ahead of the timeline and the driver reads an in-flight
/// window behind it, so the two are more than twice that window apart in the ring. At 384000
/// frames a second the window alone is 12288 frames, and a ring of 4096 wrapped the pair past
/// each other several times a second.
///
/// A timestamp also has to land on an exact multiple of this period, interpolated between two
/// isochronous completions, so a period spanning few transfers inherits the jitter of
/// individual ones. At 44100 a period of 4096 frames covered 23 transfers; at 352800 it
/// covered three, and Core Audio read a rate 7% out and spent a minute and a half walking
/// back from it. This covers twelve at 352800, which is enough.
///
/// It is also how often Core Audio hears what the clock is doing, and it will not start
/// cleanly without a few: at 32768 frames, which is three quarters of a second at 44100, the
/// host limped at a tenth of rate for a second and a half before finding its feet, and
/// everything the engine put out in the meantime had to be silence. Sizing this is a trade
/// between the two, not a maximum.
constexpr uint32_t kZeroTimestampPeriod = 16384;


/// Widest sample frame any published format uses. One source of truth with the bound
/// `BuildFormats` applies, so the ring cannot be sized for less than it publishes.
constexpr uint32_t kWidestFrameBytes = dsd::kMaxFrameBytes;
/// Bytes the ring holds.
///
/// The host treats the stream buffer as exactly one zero timestamp period of frames and
/// wraps there, whatever the buffer's length. Sizing it any other way leaves the driver
/// reading a sweep of the whole allocation while the host writes and rewrites the first
/// period of it, which is silence everywhere the two do not happen to coincide.
constexpr uint64_t kRingBytes =
    static_cast<uint64_t>(kZeroTimestampPeriod) * kWidestFrameBytes;

/// Bytes one high-speed feedback report occupies: a 16.16 fixed point count of samples per
/// microframe, little endian.
constexpr uint32_t kFeedbackBytes = 4;
/// A reported rate this far from nominal is a decoding error, not a clock, and is ignored.
constexpr double kMinFeedbackRatio = 0.95;
constexpr double kMaxFeedbackRatio = 1.05;

/// DSD silence is alternating bits, not zero: a DAC fed zeroes leaves DSD lock and pops.
constexpr uint8_t kDsdSilenceByte = 0x69;

/// UAC2 class requests, and the sampling frequency control they address.
constexpr uint8_t kUac2SetCur = 0x01;
constexpr uint8_t kUac2RangeCur = 0x02;
constexpr uint16_t kUac2SamplingFreqControl = 0x0100;
constexpr uint8_t kRequestTypeSetInterface = 0x21;
constexpr uint8_t kRequestTypeGetInterface = 0xA1;
/// A clock RANGE report is a count followed by (min, max, resolution) triples.
constexpr uint16_t kClockRangeBytes = 2 + 12 * dsd::kMaxSampleRates;

constexpr uint32_t kControlTimeoutMs = 1000;

/// One isochronous transfer's data buffer and frame list.
struct Transfer {
    IOBufferMemoryDescriptor* data;
    IOBufferMemoryDescriptor* frames;
    IOMemoryMap* data_map;
    IOMemoryMap* frame_map;
    OSAction* completion;
    /// Sample frame this transfer starts at, so its completion can be turned into a
    /// timestamp on the device's timeline.
    uint64_t start_sample;
};

}  // namespace

struct DsdAudioDriver_IVars {
    IOUSBHostDevice* device_usb;
    IOUSBHostInterface* interface;
    uint8_t configuration_value;
    IOUSBHostPipe* pipe;
    IOUSBHostPipe* feedback_pipe;

    OSSharedPtr<IOUserAudioDevice> device;
    OSSharedPtr<IOUserAudioStream> stream;
    OSSharedPtr<IOBufferMemoryDescriptor> ring;
    OSSharedPtr<IOMemoryMap> ring_map;

    dsd::Uac2Layout layout;
    dsd::FormatEntry formats[dsd::kMaxAltSettings * dsd::kMaxSampleRates];
    size_t format_count;

    Transfer transfers[kTransfersInFlight];
    Transfer feedback;
    /// Frame list entries one feedback submission uses. A feedback endpoint reports once
    /// per service interval rather than once per microframe, so this follows from its own
    /// `bInterval` and not from the output endpoint's.
    uint32_t feedback_entries;
    /// What the rate would be if the DAC ran exactly on the nominal clock, which is what the
    /// reported rate is sanity checked against.
    double nominal_samples_per_microframe;
    uint64_t feedback_reports;
    uint64_t feedback_misses;
    uint64_t feedback_completions;
    uint64_t feedback_submits;
    uint64_t feedback_rearms;
    /// Whether a feedback read is outstanding. The chain resubmits only from its own
    /// completion, so a single refused submit ends it for the rest of the track, and
    /// re-arming has to key off this rather than off whether a report was ever heard.
    bool feedback_in_flight;
    uint64_t output_completions;
    /// Bytes one sample frame occupies on the wire, from the format in force.
    uint32_t frame_bytes;
    /// Samples per microframe the current rate asks for, and the fractional carry that
    /// makes a rate which is not a whole number of samples average out exactly.
    double samples_per_microframe;
    double carry;

    /// Sample frames the ring holds at the frame size in force.
    uint64_t ring_frames;
    uint64_t next_bus_frame;
    /// Sample frames handed to the DAC since IO started. This is the timeline, and the read
    /// position in the ring is it modulo the ring length.
    uint64_t sample_counter;
    uint64_t next_timestamp_at;
    /// The previous completion, so the host time of an exact period boundary between it and
    /// the current one can be interpolated.
    uint64_t prev_sample;
    uint64_t prev_host;
    /// How far behind the submission point the ring is read. Two in-flight windows.
    ///
    /// This is the whole geometry, and it is fixed. The timeline maps a sample index to the
    /// time that index goes out on the wire, so the transfer starting at sample S carries the
    /// host's audio for index S minus this, and what is heard is this far behind the
    /// timeline's own time for that index -- which is exactly the reported latency. Nothing
    /// is measured against the host and nothing moves, so the read point cannot jump, and a
    /// jump is what a click was.
    ///
    /// One window of it is the read-ahead a transfer needs: the ring is read at submission
    /// and a transfer covers a quarter of a window, so reading level with the timeline runs
    /// its tail past what the host has written.
    ///
    /// The second window is margin, and it has to come from here because it does not come
    /// from the safety offset. Setting that to a window does not make Core Audio write a
    /// window ahead: measured, it writes between 130 and 660 frames ahead of the timeline
    /// whatever the offset says. Reading level with the timeline therefore left under one
    /// transfer of headroom, and the tail of every transfer came out as silence -- heard as
    /// noise, not as a dropout.
    uint64_t read_lag;
    /// The last write the host made, which bounds the read: audio it has not written yet
    /// does not exist, and silence is the honest thing to send in its place.
    uint64_t last_write_sample;
    uint32_t last_write_frames;
    /// One-time shift of the read point, set from the host's first write of a stream.
    ///
    /// The lag above says how far behind the timeline to read. Where the timeline itself
    /// starts is a guess: `StartIsoc` takes it from the last zero timestamp posted, which is
    /// the only thing that survives a stream ending, and which the host does not have to
    /// agree with. Measured, it disagrees by more than a whole ring after the engine has been
    /// torn down for a while -- the session opens already lapped and stays that way.
    ///
    /// So the guess is corrected once, against the only authority on where the host is
    /// writing: the host. Nothing real has been read at that point -- the read is bounded by
    /// `last_write_frames`, which is zero until this same cycle -- so the shift is inaudible,
    /// and after it the geometry is as fixed as it ever was. This is not the anchoring that
    /// caused clicks: that one ran for the life of the stream and moved the read point under
    /// playing audio.
    int64_t read_offset;
    /// Whether that correction has been applied for this stream.
    bool timeline_anchored;
    /// Frames sent as silence because the host had not written that far yet.
    uint64_t starved;
    /// The byte this carrier calls silence: DSD silence is alternating bits, not zero.
    uint8_t silence_byte;
    /// IO operations the host has performed, so a sample of them can be logged.
    uint64_t io_calls;
    uint64_t timestamps_posted;
    /// Times the host wrote at or behind where the engine was already reading, which is the
    /// ring lapping and is audible. Counted rather than logged per occurrence: the IO handler
    /// runs hundreds of times a second.
    uint64_t crossings;
    /// Times the host had run a whole ring ahead of the read point, so the slot the engine
    /// was about to read had already been overwritten.
    uint64_t laps;
    bool running;
    uint8_t active_alt;
    /// The rate the engine is streaming, so a client asking for the same one can be let
    /// straight in rather than restarted.
    uint32_t active_rate;
    /// Whether a client is doing IO. The engine outlives this: it keeps streaming silence
    /// and posting timestamps between clients, so the next one starts against a clock that
    /// never stopped. It does not outlive it indefinitely -- see `idle_completions`.
    bool client_active;

    /// Where the ring is read for a point on the timeline. One definition, because the read
    /// path and the counters that police it have to mean the same thing by it.
    uint64_t ReadPointFor(uint64_t timeline) const {
        const int64_t at =
            static_cast<int64_t>(timeline) - static_cast<int64_t>(read_lag) + read_offset;
        return at > 0 ? static_cast<uint64_t>(at) : 0;
    }

    /// Output completions since the last client stopped, which is how long the engine has
    /// been idling. Reset whenever a client is doing IO.
    uint64_t idle_completions;
    /// A teardown is scheduled and the chains are draining, so nothing resubmits. Cleared by
    /// `StopIsoc`, which is what the teardown runs.
    bool idle_stopping;
};

namespace {

/// Send a UAC2 class request to the clock source on the audio control interface.
kern_return_t ClockRequest(IOUSBHostInterface* interface, const dsd::Uac2Layout& layout,
                           uint8_t direction, uint8_t request, IOMemoryDescriptor* buffer,
                           uint16_t length, uint16_t* transferred) {
    const uint16_t index =
        static_cast<uint16_t>((static_cast<uint16_t>(layout.clock_id) << 8) |
                              layout.control_interface);
    return interface->DeviceRequest(direction, request, kUac2SamplingFreqControl, index, length,
                                    buffer, transferred, kControlTimeoutMs);
}

/// Read the clock's RANGE report, which is the widest set of rates the device admits to.
///
/// This is the same report src/output/usb/device.rs reads, and it is deliberately not the
/// Core Audio format list: the intersection Core Audio derives is sometimes narrower than
/// what the clock accepts, and a driver that owns the interface has no reason to inherit
/// that narrowing.
size_t ReadClockRates(IOUSBHostInterface* interface, const dsd::Uac2Layout& layout,
                      uint32_t* rates, size_t capacity) {
    OSSharedPtr<IOBufferMemoryDescriptor> buffer;
    IOBufferMemoryDescriptor* raw = nullptr;
    if (IOBufferMemoryDescriptor::Create(kIOMemoryDirectionInOut, kClockRangeBytes, 8, &raw) !=
        kIOReturnSuccess) {
        return 0;
    }
    buffer.reset(raw, OSNoRetain);

    uint16_t transferred = 0;
    if (ClockRequest(interface, layout, kRequestTypeGetInterface, kUac2RangeCur, buffer.get(),
                     kClockRangeBytes, &transferred) != kIOReturnSuccess) {
        return 0;
    }

    OSSharedPtr<IOMemoryMap> map;
    IOMemoryMap* raw_map = nullptr;
    if (buffer->CreateMapping(0, 0, 0, 0, 0, &raw_map) != kIOReturnSuccess) {
        return 0;
    }
    map.reset(raw_map, OSNoRetain);
    const uint8_t* report = reinterpret_cast<const uint8_t*>(map->GetAddress());
    return dsd::ParseClockRange(report, transferred, rates, capacity);
}

kern_return_t SetClockRate(IOUSBHostInterface* interface, const dsd::Uac2Layout& layout,
                           uint32_t rate) {
    OSSharedPtr<IOBufferMemoryDescriptor> buffer;
    IOBufferMemoryDescriptor* raw = nullptr;
    kern_return_t result = IOBufferMemoryDescriptor::Create(kIOMemoryDirectionInOut, 4, 4, &raw);
    if (result != kIOReturnSuccess) {
        return result;
    }
    buffer.reset(raw, OSNoRetain);

    OSSharedPtr<IOMemoryMap> map;
    IOMemoryMap* raw_map = nullptr;
    result = buffer->CreateMapping(0, 0, 0, 0, 0, &raw_map);
    if (result != kIOReturnSuccess) {
        return result;
    }
    map.reset(raw_map, OSNoRetain);

    uint8_t* bytes = reinterpret_cast<uint8_t*>(map->GetAddress());
    bytes[0] = static_cast<uint8_t>(rate);
    bytes[1] = static_cast<uint8_t>(rate >> 8);
    bytes[2] = static_cast<uint8_t>(rate >> 16);
    bytes[3] = static_cast<uint8_t>(rate >> 24);

    uint16_t transferred = 0;
    return ClockRequest(interface, layout, kRequestTypeSetInterface, kUac2SetCur, buffer.get(), 4,
                        &transferred);
}

/// Describe one publishable format to Core Audio.
///
/// Native DSD has no format ID of its own, so it goes out as big-endian 32-bit integer PCM,
/// which is the same convention ALSA calls DSD_U32_BE. The non-mixable flag is what keeps
/// the bits intact: it stops the HAL converting or mixing anything into the stream.
IOUserAudioStreamBasicDescription Describe(const dsd::FormatEntry& entry, bool mixable) {
    IOUserAudioStreamBasicDescription format{};
    format.mSampleRate = entry.sample_rate;
    format.mFormatID = IOUserAudioFormatID::LinearPCM;

    // Each PCM format is published twice, mixable and not. The non-mixable one is what a
    // player after a bit-perfect path asks for. The mixable one is what the HAL needs to use
    // the device for ordinary output at all: with only non-mixable formats Core Audio will
    // not even keep the device as the default output, and anything that just plays to the
    // default plays somewhere else.
    uint32_t flags = IOUserAudioFormatFlags::FormatFlagIsSignedInteger;
    if (!mixable) {
        flags |= IOUserAudioFormatFlags::FormatFlagIsNonMixable;
    }
    // Big endian is what separates native DSD from PCM of the same width, both here and in
    // the lookup StartDevice does.
    if (entry.native_dsd) {
        flags |= IOUserAudioFormatFlags::FormatFlagIsBigEndian;
    }
    const uint32_t subslot_bits = static_cast<uint32_t>(entry.subslot_bytes) * 8;
    // A resolution narrower than its subslot is left justified in it, which is what UAC2
    // means by 24 bits in a four byte subslot.
    flags |= entry.bit_resolution == subslot_bits
                 ? IOUserAudioFormatFlags::FormatFlagIsPacked
                 : IOUserAudioFormatFlags::FormatFlagIsAlignedHigh;
    format.mFormatFlags = static_cast<IOUserAudioFormatFlags>(flags);

    const uint32_t frame_bytes = static_cast<uint32_t>(entry.channels) * entry.subslot_bytes;
    format.mBytesPerPacket = frame_bytes;
    format.mFramesPerPacket = 1;
    format.mBytesPerFrame = frame_bytes;
    format.mChannelsPerFrame = entry.channels;
    format.mBitsPerChannel = entry.bit_resolution;
    return format;
}

}  // namespace

namespace {

/// Copy a USB string descriptor into `out` as ASCII, replacing anything outside it.
///
/// USB string descriptors are UTF-16LE. Device names are ASCII in practice, and a name is
/// only ever shown, so a byte that is not is written as '?' rather than dropping the name.
void CopyAscii(const IOUSBStringDescriptor* descriptor, char* out, size_t capacity) {
    out[0] = '\0';
    if (descriptor == nullptr || descriptor->bLength < 4 || capacity < 2) {
        return;
    }
    const size_t units = (descriptor->bLength - 2) / 2;
    const size_t limit = units < capacity - 1 ? units : capacity - 1;
    for (size_t index = 0; index < limit; index++) {
        const uint16_t unit = static_cast<uint16_t>(
            descriptor->bString[index * 2] | (descriptor->bString[index * 2 + 1] << 8));
        out[index] = (unit >= 0x20 && unit < 0x7F) ? static_cast<char>(unit) : '?';
    }
    out[limit] = '\0';
}

/// Name the DAC and give it a UID stable across replug, so Core Audio remembers which
/// device a user picked.
void IdentifyDevice(IOUSBHostInterface* interface, char* name, size_t name_capacity, char* uid,
                    size_t uid_capacity) {
    snprintf(name, name_capacity, "DSD USB Audio");
    snprintf(uid, uid_capacity, "dsd-rust:unknown");

    IOUSBHostDevice* raw_device = nullptr;
    if (interface->CopyDevice(&raw_device) != kIOReturnSuccess || raw_device == nullptr) {
        return;
    }
    OSSharedPtr<IOUSBHostDevice> device(raw_device, OSNoRetain);

    const IOUSBDeviceDescriptor* descriptor = device->CopyDeviceDescriptor();
    if (descriptor == nullptr) {
        return;
    }
    snprintf(uid, uid_capacity, "dsd-rust:%04x:%04x", descriptor->idVendor, descriptor->idProduct);
    if (descriptor->iProduct != 0) {
        const IOUSBStringDescriptor* product = device->CopyStringDescriptor(descriptor->iProduct);
        if (product != nullptr) {
            CopyAscii(product, name, name_capacity);
            IOUSBHostFreeDescriptor(product);
        }
    }
    IOUSBHostFreeDescriptor(descriptor);
}

}  // namespace

bool DsdAudioDriver::init() {
    if (!super::init()) {
        return false;
    }
    ivars = IONewZero(DsdAudioDriver_IVars, 1);
    return ivars != nullptr;
}

void DsdAudioDriver::free() {
    if (ivars != nullptr) {
        ivars->device.reset();
        ivars->stream.reset();
        ivars->ring.reset();
        ivars->ring_map.reset();
        OSSafeReleaseNULL(ivars->pipe);
        OSSafeReleaseNULL(ivars->feedback_pipe);
        OSSafeReleaseNULL(ivars->interface);
        OSSafeReleaseNULL(ivars->device_usb);
    }
    IOSafeDeleteNULL(ivars, DsdAudioDriver_IVars, 1);
    super::free();
}

namespace {

/// Read the device's configuration descriptor and find its streaming interface in it.
bool ReadUsbLayout(DsdAudioDriver_IVars* ivars) {
    const IOUSBConfigurationDescriptor* config = ivars->device_usb->CopyConfigurationDescriptor(static_cast<uint8_t>(0));
    if (config == nullptr) {
        Log("no configuration descriptor");
        return false;
    }
    ivars->configuration_value = config->bConfigurationValue;
    const bool parsed = dsd::ParseLayout(reinterpret_cast<const uint8_t*>(config),
                                         config->wTotalLength, &ivars->layout);
    IOUSBHostFreeDescriptor(config);
    if (!parsed) {
        Log("device carries no streaming alternate setting with a clock");
        return false;
    }
    return true;
}

/// Take the streaming interface for ourselves.
///
/// Setting the configuration without registering its interfaces for matching is what
/// excludes usbaudiod: the nubs exist for us to iterate, but the daemon never sees them
/// published, so there is no window to race and nothing to take away from it afterwards.
bool ClaimStreamingInterface(DsdAudioDriver* driver, DsdAudioDriver_IVars* ivars) {
    kern_return_t result = ivars->device_usb->SetConfiguration(ivars->configuration_value, false);
    if (result != kIOReturnSuccess) {
        Log("cannot set configuration %u: 0x%08x", ivars->configuration_value, result);
        return false;
    }

    uintptr_t iterator = 0;
    result = ivars->device_usb->CreateInterfaceIterator(&iterator);
    if (result != kIOReturnSuccess) {
        Log("cannot iterate interfaces: 0x%08x", result);
        return false;
    }
    IOUSBHostInterface* candidate = nullptr;
    while (ivars->device_usb->CopyInterface(iterator, &candidate) == kIOReturnSuccess &&
           candidate != nullptr) {
        const IOUSBConfigurationDescriptor* config = candidate->CopyConfigurationDescriptor();
        const IOUSBInterfaceDescriptor* descriptor =
            config != nullptr ? candidate->GetInterfaceDescriptor(config) : nullptr;
        const bool wanted =
            descriptor != nullptr && descriptor->bInterfaceNumber == ivars->layout.streaming_interface;
        if (config != nullptr) {
            IOUSBHostFreeDescriptor(config);
        }
        if (wanted) {
            ivars->interface = candidate;
            break;
        }
        OSSafeReleaseNULL(candidate);
    }
    ivars->device_usb->DestroyInterfaceIterator(iterator);

    if (ivars->interface == nullptr) {
        Log("device has no interface numbered %u", ivars->layout.streaming_interface);
        return false;
    }
    result = ivars->interface->Open(driver, 0, nullptr);
    if (result != kIOReturnSuccess) {
        Log("cannot open the streaming interface: 0x%08x", result);
        return false;
    }
    return true;
}

/// Read the clock's rates and turn them, with the alternate settings, into the format list.
bool ReadRatesAndFormats(DsdAudioDriver_IVars* ivars) {
    uint32_t rates[dsd::kMaxSampleRates] = {};
    const size_t rate_count =
        ReadClockRates(ivars->interface, ivars->layout, rates, dsd::kMaxSampleRates);
    if (rate_count == 0) {
        Log("clock source reported no rates");
        return false;
    }

    constexpr size_t kCapacity = sizeof(ivars->formats) / sizeof(ivars->formats[0]);
    ivars->format_count =
        dsd::BuildFormats(ivars->layout, rates, rate_count, ivars->formats, kCapacity);
    if (ivars->format_count == 0) {
        Log("no rate fits any alternate setting");
        return false;
    }
    Log("%zu formats over %zu alternate settings", ivars->format_count, ivars->layout.alt_count);
    return true;
}


}  // namespace

kern_return_t IMPL(DsdAudioDriver, Start) {
    kern_return_t result = Start(provider, SUPERDISPATCH);
    if (result != kIOReturnSuccess) {
        return result;
    }

    // The provider is the whole device, not one interface. macOS will not hand a third
    // party driver an audio class interface -- usbaudiod is published against those before
    // anyone else can match them -- so the driver takes the device and configures it itself.
    ivars->device_usb = OSDynamicCast(IOUSBHostDevice, provider);
    if (ivars->device_usb == nullptr) {
        Log("provider is not an IOUSBHostDevice");
        return kIOReturnNoDevice;
    }
    ivars->device_usb->retain();

    result = ivars->device_usb->Open(this, 0, 0);
    if (result != kIOReturnSuccess) {
        Log("cannot open the device: 0x%08x", result);
        return result;
    }
    if (!ReadUsbLayout(ivars) || !ClaimStreamingInterface(this, ivars) ||
        !ReadRatesAndFormats(ivars)) {
        return kIOReturnUnsupported;
    }

    result = PublishAudioObjects();
    if (result != kIOReturnSuccess) {
        Log("publishing the audio device failed: 0x%08x", result);
        return result;
    }

    Log("published, registering");
    RegisterService();
    return kIOReturnSuccess;
}

kern_return_t IMPL(DsdAudioDriver, Stop) {
    StopIsoc();
    if (ivars->device) {
        RemoveObject(ivars->device.get());
    }
    if (ivars->interface != nullptr) {
        ivars->interface->SelectAlternateSetting(0);
        ivars->interface->Close(this, 0);
    }
    if (ivars->device_usb != nullptr) {
        ivars->device_usb->Close(this, 0);
    }
    return Stop(provider, SUPERDISPATCH);
}

kern_return_t IMPL(DsdAudioDriver, NewUserClient) {
    return NewUserClient(in_type, out_user_client, SUPERDISPATCH);
}

kern_return_t DsdAudioDriver::PublishAudioObjects() {
    char name[64] = {};
    char uid[64] = {};
    IdentifyDevice(ivars->interface, name, sizeof(name), uid, sizeof(uid));

    OSSharedPtr<OSString> device_uid = OSSharedPtr(OSString::withCString(uid), OSNoRetain);
    OSSharedPtr<OSString> model_uid =
        OSSharedPtr(OSString::withCString("dsd-rust:model"), OSNoRetain);
    OSSharedPtr<OSString> manufacturer =
        OSSharedPtr(OSString::withCString("dsd-rust"), OSNoRetain);

    ivars->device = IOUserAudioDevice::Create(this, false, device_uid.get(), model_uid.get(),
                                              manufacturer.get(), kZeroTimestampPeriod);
    if (!ivars->device) {
        Log("IOUserAudioDevice::Create failed");
        return kIOReturnNoMemory;
    }
    OSSharedPtr<OSString> device_name = OSSharedPtr(OSString::withCString(name), OSNoRetain);
    ivars->device->SetName(device_name.get());
    ivars->device->SetTransportType(IOUserAudioTransportType::USB);
    ivars->device->SetCanBeDefaultOutputDevice(true);
    ivars->device->SetCanBeDefaultSystemOutputDevice(true);

    IOBufferMemoryDescriptor* ring = nullptr;
    kern_return_t result =
        IOBufferMemoryDescriptor::Create(kIOMemoryDirectionInOut, kRingBytes, 4096, &ring);
    if (result != kIOReturnSuccess) {
        Log("ring allocation failed: 0x%08x", result);
        return result;
    }
    ivars->ring.reset(ring, OSNoRetain);

    IOMemoryMap* ring_map = nullptr;
    result = ivars->ring->CreateMapping(0, 0, 0, 0, 0, &ring_map);
    if (result != kIOReturnSuccess) {
        Log("ring mapping failed: 0x%08x", result);
        return result;
    }
    ivars->ring_map.reset(ring_map, OSNoRetain);

    ivars->stream = IOUserAudioStream::Create(this, IOUserAudioStreamDirection::Output,
                                              ivars->ring.get());
    if (!ivars->stream) {
        Log("IOUserAudioStream::Create failed");
        return kIOReturnNoMemory;
    }
    ivars->stream->SetTerminalType(IOUserAudioStreamTerminalType::Headphones);
    // Without this the host has a device it will not do IO on: a player that opens the
    // device itself still reaches StartDevice, but anything that just picks the default
    // output plays into nothing.
    ivars->stream->SetStreamIsActive(true);

    IOUserAudioStreamBasicDescription formats[dsd::kMaxAltSettings * dsd::kMaxSampleRates];
    double rates[dsd::kMaxSampleRates];
    size_t rate_count = 0;
    size_t published = 0;
    for (size_t index = 0; index < ivars->format_count; index++) {
        const dsd::FormatEntry& entry = ivars->formats[index];
        // Mixable first, so the format the device settles on is one the HAL can use.
        if (!entry.native_dsd) {
            formats[published++] = Describe(entry, true);
        }
        formats[published++] = Describe(entry, false);
        bool seen = false;
        for (size_t known = 0; known < rate_count; known++) {
            seen = seen || rates[known] == entry.sample_rate;
        }
        if (!seen && rate_count < dsd::kMaxSampleRates) {
            rates[rate_count++] = entry.sample_rate;
        }
    }
    // The head of the list is the default: the widest PCM the DAC accepts, at the lowest rate
    // it reports. Everything the OS plays goes out at this width until something changes it,
    // and the narrowest is a poor thing to hand a DAC by default.
    const IOUserAudioStreamBasicDescription& fallback = formats[0];
    Log("publishing %zu stream formats over %zu rates, default %.0f Hz %u bit in %u bytes",
        published, rate_count, fallback.mSampleRate, fallback.mBitsPerChannel,
        fallback.mBytesPerFrame / (fallback.mChannelsPerFrame != 0 ? fallback.mChannelsPerFrame : 1));
    ivars->stream->SetAvailableStreamFormats(formats, static_cast<uint32_t>(published));
    ivars->stream->SetCurrentStreamFormat(&fallback);
    ivars->device->SetAvailableSampleRates(rates, rate_count);
    ivars->device->SetSampleRate(fallback.mSampleRate);

    result = ivars->device->AddStream(ivars->stream.get());
    if (result != kIOReturnSuccess) {
        Log("AddStream failed: 0x%08x", result);
        return result;
    }
    // Output IO needs no work here: the host writes into the ring the stream was created
    // over, and the isochronous engine reads it at the position its own timeline names. The
    // sample time it reports is the only view of where the host thinks it is writing, so a
    // sample of them is logged against where the engine is reading.
    DsdAudioDriver_IVars* state = ivars;
    ivars->device->SetIOOperationHandler(
        ^kern_return_t(IOUserAudioObjectID, IOUserAudioIOOperation in_operation,
                       uint32_t in_frame_size, uint64_t in_sample_time, uint64_t) {
            state->io_calls++;
            if (in_operation == IOUserAudioIOOperationWriteEnd && !state->timeline_anchored) {
                // The host's first write of the stream, and the first statement of where it
                // actually is. Put the read point half a ring behind it, which is the midpoint
                // of the band the two failures bound: the host falling onto the read point at
                // zero, and lapping it at a ring less one IO buffer. That is the most headroom
                // available in both directions at once, it is rate independent because the
                // ring is a fixed number of frames, and it is about where sessions that were
                // never torn down settled on their own -- measured at 9.4k on a 16384 ring.
                //
                // Nothing is gated on this. Until it happens the offset is zero, which is
                // what the read point was before, and the read is bounded by the host's own
                // last write in either case -- so a stream where this never fires plays as
                // it always did rather than falling silent.
                const uint64_t unshifted = state->ReadPointFor(state->sample_counter);
                const int64_t target = static_cast<int64_t>(state->ring_frames / 2);
                state->read_offset =
                    static_cast<int64_t>(in_sample_time) - static_cast<int64_t>(unshifted) -
                    target;
                state->timeline_anchored = true;
                Log("anchored the read point on the host's first write at %llu: was %llu, "
                    "shifted by %lld to sit %lld behind",
                    in_sample_time, unshifted, state->read_offset, target);
            }
            const uint64_t read_at = state->ReadPointFor(state->sample_counter);
            if (in_operation == IOUserAudioIOOperationWriteEnd) {
                // Both of these are impossible if the geometry holds, which is why they are
                // counted rather than corrected. The host can fall back onto the read point,
                // or it can get a whole ring ahead of it and overwrite the slot the engine is
                // about to read -- and the second one sounds perfectly fine, because what
                // comes out is continuous audio from further along the track. It is heard as
                // the sound running ahead of the picture, not as a glitch, which is why it
                // went unnoticed for as long as it did. Either means the safety offset is not
                // buying what it is supposed to.
                const int64_t margin =
                    static_cast<int64_t>(in_sample_time) - static_cast<int64_t>(read_at);
                const int64_t lapped_at = static_cast<int64_t>(state->ring_frames) -
                                          static_cast<int64_t>(in_frame_size);
                if (margin <= 0) {
                    state->crossings++;
                }
                if (margin >= lapped_at) {
                    state->laps++;
                }
                state->last_write_sample = in_sample_time;
                state->last_write_frames = in_frame_size;
            }
            // The first cycle of a session says whether the host and the engine agree on
            // where the timeline is. They are on the same one only if the driver resumed
            // where Core Audio left off, and a start that disagrees is audible.
            if (state->io_calls == 1) {
                Log("first IO: op %u, %u frames at sample %llu; engine queues at %llu, reads at "
                    "%llu, so the host leads the read point by %lld",
                    in_operation, in_frame_size, in_sample_time, state->sample_counter, read_at,
                    static_cast<int64_t>(in_sample_time - read_at));
            }
            if (state->io_calls % 250 == 0 && in_operation == IOUserAudioIOOperationWriteEnd) {
                Log("host wrote %u frames at sample %llu (ring slot %llu); engine reads at "
                    "sample %llu (slot %llu), so the host leads it by %lld",
                    in_frame_size, in_sample_time,
                    state->ring_frames != 0 ? in_sample_time % state->ring_frames : 0, read_at,
                    state->ring_frames != 0 ? read_at % state->ring_frames : 0,
                    static_cast<int64_t>(in_sample_time) - static_cast<int64_t>(read_at));
            }
            return kIOReturnSuccess;
        });
    result = AddObject(ivars->device.get());
    if (result != kIOReturnSuccess) {
        Log("AddObject failed: 0x%08x", result);
        return result;
    }
    // Again, now the device is registered. Set before that, the format does not survive
    // Core Audio picking the device up: it comes back as the narrowest the DAC publishes,
    // whatever the stream was told beforehand.
    ivars->stream->SetCurrentStreamFormat(&fallback);
    const IOUserAudioStreamBasicDescription settled = ivars->stream->GetCurrentStreamFormat();
    Log("stream settled on %.0f Hz %u bit in %u bytes", settled.mSampleRate,
        settled.mBitsPerChannel,
        settled.mBytesPerFrame / (settled.mChannelsPerFrame != 0 ? settled.mChannelsPerFrame : 1));
    return result;
}

namespace {

/// Copy `frames` sample frames out of the ring at `position`, wrapping once at the end.
void CopyFromRing(const uint8_t* ring, uint64_t ring_frames, uint64_t position, uint32_t frames,
                  uint32_t stride, uint8_t* out) {
    if (ring_frames == 0 || frames == 0) {
        return;
    }
    uint64_t at = position % ring_frames;
    uint32_t remaining = frames;
    while (remaining > 0) {
        const uint64_t available = ring_frames - at;
        const uint32_t run = remaining < available ? remaining : static_cast<uint32_t>(available);
        memcpy(out, ring + at * stride, static_cast<size_t>(run) * stride);
        out += static_cast<size_t>(run) * stride;
        remaining -= run;
        at = (at + run) % ring_frames;
    }
}

void FreeTransfer(Transfer& transfer) {
    OSSafeReleaseNULL(transfer.completion);
    OSSafeReleaseNULL(transfer.data_map);
    OSSafeReleaseNULL(transfer.frame_map);
    OSSafeReleaseNULL(transfer.data);
    OSSafeReleaseNULL(transfer.frames);
}

void FreeTransfers(DsdAudioDriver_IVars* ivars) {
    for (uint32_t index = 0; index < kTransfersInFlight; index++) {
        FreeTransfer(ivars->transfers[index]);
    }
    FreeTransfer(ivars->feedback);
}

/// Buffers and a frame list for one transfer, without its completion action.
kern_return_t AllocateBuffers(Transfer& transfer, IOOptionBits direction, uint64_t data_bytes) {
    const uint64_t frame_bytes =
        static_cast<uint64_t>(kMicroframesPerTransfer) * sizeof(IOUSBIsochronousFrame);
    kern_return_t result =
        IOBufferMemoryDescriptor::Create(direction, data_bytes, 4096, &transfer.data);
    if (result != kIOReturnSuccess) {
        return result;
    }
    result = IOBufferMemoryDescriptor::Create(kIOMemoryDirectionInOut, frame_bytes, 8,
                                              &transfer.frames);
    if (result != kIOReturnSuccess) {
        return result;
    }
    result = transfer.data->CreateMapping(0, 0, 0, 0, 0, &transfer.data_map);
    if (result != kIOReturnSuccess) {
        return result;
    }
    return transfer.frames->CreateMapping(0, 0, 0, 0, 0, &transfer.frame_map);
}

/// Give every transfer its own data buffer, frame list and completion action.
///
/// Each buffer is sized for the endpoint's largest packet in every microframe it covers, so
/// a rate that is not a whole number of samples per microframe still fits its long frames.
kern_return_t AllocateTransfers(DsdAudioDriver* driver, DsdAudioDriver_IVars* ivars,
                                uint32_t max_packet) {
    FreeTransfers(ivars);
    const uint64_t data_bytes = static_cast<uint64_t>(kMicroframesPerTransfer) * max_packet;

    for (uint32_t index = 0; index < kTransfersInFlight; index++) {
        Transfer& transfer = ivars->transfers[index];
        kern_return_t result = AllocateBuffers(transfer, kIOMemoryDirectionOut, data_bytes);
        if (result != kIOReturnSuccess) {
            return result;
        }
        // No reference storage: the action the pipe hands back to the completion does not
        // carry one, and OSAction::GetReference asserts rather than returning null, which
        // takes the whole machine down through the crash-too-many-times panic. The
        // completion identifies its transfer by matching the action instead.
        result = driver->CreateActionIsochComplete(0, &transfer.completion);
        if (result != kIOReturnSuccess) {
            return result;
        }
    }

    const uint64_t feedback_bytes =
        static_cast<uint64_t>(kMicroframesPerTransfer) * kFeedbackBytes;
    const kern_return_t result =
        AllocateBuffers(ivars->feedback, kIOMemoryDirectionIn, feedback_bytes);
    if (result != kIOReturnSuccess) {
        return result;
    }
    // The same action type as an output transfer. Both completions arrive through
    // IOUSBHostPipe::CompleteAsyncIsochIO, and a second method declared with that type is
    // never dispatched to, so one handler tells them apart by which action came back.
    return driver->CreateActionIsochComplete(0, &ivars->feedback.completion);
}

}  // namespace

kern_return_t DsdAudioDriver::StartDevice(IOUserAudioObjectID in_object_id,
                                          IOUserAudioStartStopFlags in_flags) {
    if (!ivars->stream || !ivars->ring_map || ivars->interface == nullptr) {
        Log("asked to start IO before the device was published");
        return kIOReturnNotReady;
    }
    const IOUserAudioStreamBasicDescription format = ivars->stream->GetCurrentStreamFormat();
    // Rate, frame width and bit depth do not separate native DSD from 32 bit PCM: at any
    // rate the two agree on all three. What tells them apart is the big endian flag native
    // carries, so the lookup has to read it. Getting this wrong puts the DAC into raw DSD
    // mode at twice the nominal rate and feeds it DoP words, which is audible as ticks.
    const bool wants_native =
        (format.mFormatFlags & IOUserAudioFormatFlags::FormatFlagIsBigEndian) != 0;
    const dsd::FormatEntry* entry = nullptr;
    for (size_t index = 0; index < ivars->format_count; index++) {
        const dsd::FormatEntry& candidate = ivars->formats[index];
        const uint32_t frame_bytes =
            static_cast<uint32_t>(candidate.channels) * candidate.subslot_bytes;
        if (candidate.sample_rate == format.mSampleRate && frame_bytes == format.mBytesPerFrame &&
            candidate.bit_resolution == format.mBitsPerChannel &&
            candidate.native_dsd == wants_native) {
            entry = &candidate;
            break;
        }
    }
    if (entry == nullptr) {
        Log("no alternate setting carries the format the host asked for");
        return kIOReturnUnsupported;
    }

    const uint32_t frame_bytes = static_cast<uint32_t>(entry->channels) * entry->subslot_bytes;
    // Let a client that wants what is already streaming straight in.
    //
    // Tearing the engine down on every StopDevice made each client pay a cold start: about
    // a second in which Core Audio has not found its rate, the engine has nothing real to
    // send, and the frames the host writes in the meantime are never read. A browser opens
    // and closes the stream several times while a page settles, so it paid that repeatedly,
    // and its video pipeline waited on an audio clock that had run through all of it.
    //
    // The engine kept running, so the timeline, the ring geometry and the read point are all
    // still where they were. There is nothing to set up and nothing to wait for.
    // Not one whose teardown is already scheduled: its chains have stopped resubmitting, so
    // there is nothing left running to join. That falls through to a full restart below,
    // where `StopIsoc` clears the flag and the scheduled teardown becomes a no-op.
    if (ivars->running && !ivars->idle_stopping && ivars->active_alt == entry->alt_setting &&
        ivars->active_rate == static_cast<uint32_t>(entry->sample_rate) &&
        ivars->frame_bytes == frame_bytes) {
        // The read point stays where it is -- that is the whole point -- but the counters
        // that describe a start are per client, so they begin again here.
        ivars->crossings = 0;        ivars->laps = 0;
        ivars->starved = 0;
        ivars->idle_completions = 0;
        ivars->client_active = true;
        Log("client starts on the engine already streaming %u Hz on alt %u",
            ivars->active_rate, ivars->active_alt);
        return super::StartDevice(in_object_id, in_flags);
    }
    // The driver reads the ring up to a whole in-flight window ahead of what the DAC is
    // playing, because that much audio is already handed to the controller. The host writes
    // relative to the timeline the zero timestamps describe, so unless it is told to stay
    // that far ahead it writes behind the read point and the DAC gets an empty ring.
    // Twice the in-flight window, not once. At exactly one window the host's writes land
    // level with the read point and every transfer picks up whatever was there before, so
    // the margin has to cover the whole window again plus the host's own IO buffer.
    const uint32_t window_frames = static_cast<uint32_t>(
        kTransfersInFlight * kMicroframesPerTransfer * entry->sample_rate / 8000.0);
    ivars->read_lag = static_cast<uint64_t>(window_frames) * 2;
    // The latency is the read lag and nothing else. The timeline maps a sample index to the
    // time that index goes out on the wire, so the in-flight window a transfer spends on the
    // controller is already inside it and adding it again would report it twice. The host's
    // sample X is carried by the transfer starting at X plus the lag, so it is heard that
    // much after the timeline's own time for X. Video sync is what this number is for, and
    // it is now a constant the driver can state rather than a consequence of where the host
    // happened to be writing when the read point was last set.
    ivars->device->SetOutputSafetyOffset(window_frames);
    ivars->device->SetOutputLatency(static_cast<uint32_t>(ivars->read_lag));
    Log("host asked for %.0f Hz %{public}s, alternate setting %u, in-flight window %u frames",
        entry->sample_rate, entry->native_dsd ? "native DSD" : "PCM", entry->alt_setting,
        window_frames);
    // A different format than the engine is on, so it does have to be restarted.
    StopIsoc();
    const kern_return_t result = StartIsoc(static_cast<uint32_t>(entry->sample_rate),
                                           entry->alt_setting, frame_bytes);
    if (result != kIOReturnSuccess) {
        StopIsoc();
        return result;
    }
    ivars->client_active = true;
    return super::StartDevice(in_object_id, in_flags);
}

kern_return_t DsdAudioDriver::StopDevice(IOUserAudioObjectID in_object_id,
                                         IOUserAudioStartStopFlags in_flags) {
    // Leave the engine streaming. A running audio device keeps its clock running whether or
    // not anything is playing, and stopping it here is what made every client pay a cold
    // start. The read path sends silence while no one is writing, which for a DSD carrier
    // is what holds the DAC in lock between tracks as well.
    //
    // Leaving it streaming is not leaving it streaming forever. The completion handler
    // counts how long it runs with no client and retires it past `kIdleTeardownMs`, which is
    // long after the gap between two tracks and long before the streaming is worth its cost.
    //
    // It is also torn down on a format change, in StartDevice, and when the driver stops.
    ivars->client_active = false;
    ivars->idle_completions = 0;
    Log("client stops: %llu cycles the engine had overtaken the host, %llu cycles the host "
        "had lapped the read point, %llu frames sent as silence",
        ivars->crossings, ivars->laps, ivars->starved);
    return super::StopDevice(in_object_id, in_flags);
}

kern_return_t DsdAudioDriver::StartIsoc(uint32_t rate, uint8_t alt_setting, uint32_t frame_bytes) {
    const dsd::AltSetting* alt = nullptr;
    for (size_t index = 0; index < ivars->layout.alt_count; index++) {
        if (ivars->layout.alts[index].alt_setting == alt_setting) {
            alt = &ivars->layout.alts[index];
            break;
        }
    }
    if (alt == nullptr) {
        return kIOReturnUnsupported;
    }

    kern_return_t result = ivars->interface->SelectAlternateSetting(alt_setting);
    if (result != kIOReturnSuccess) {
        Log("cannot select alternate setting %u: 0x%08x", alt_setting, result);
        return result;
    }
    ivars->active_alt = alt_setting;

    result = SetClockRate(ivars->interface, ivars->layout, rate);
    if (result != kIOReturnSuccess) {
        Log("cannot set the clock to %u Hz: 0x%08x", rate, result);
        return result;
    }

    IOUSBHostPipe* pipe = nullptr;
    result = ivars->interface->CopyPipe(alt->out_endpoint, &pipe);
    if (result != kIOReturnSuccess) {
        Log("cannot open the output pipe: 0x%08x", result);
        return result;
    }
    OSSafeReleaseNULL(ivars->pipe);
    ivars->pipe = pipe;

    OSSafeReleaseNULL(ivars->feedback_pipe);
    if (alt->feedback_endpoint != 0) {
        IOUSBHostPipe* feedback = nullptr;
        if (ivars->interface->CopyPipe(alt->feedback_endpoint, &feedback) == kIOReturnSuccess) {
            ivars->feedback_pipe = feedback;
        } else {
            Log("no feedback pipe on endpoint 0x%02x, running open loop", alt->feedback_endpoint);
        }
    }

    // Belt and braces against the ring's stride. `BuildFormats` already refuses to publish a
    // format this wide, so reaching here means the format list and the ring disagree, and the
    // read path would run off the end of the mapping rather than report anything.
    if (frame_bytes == 0 || frame_bytes > kWidestFrameBytes) {
        Log("refusing a %u byte frame against a ring of stride %u", frame_bytes,
            kWidestFrameBytes);
        return kIOReturnBadArgument;
    }
    ivars->frame_bytes = frame_bytes;
    // Not the mapping length over the frame width: the host wraps at the zero timestamp
    // period regardless of how much memory the buffer actually holds.
    ivars->ring_frames = kZeroTimestampPeriod;
    ivars->samples_per_microframe = static_cast<double>(rate) / 8000.0;
    ivars->nominal_samples_per_microframe = ivars->samples_per_microframe;
    ivars->feedback_reports = 0;
    ivars->feedback_misses = 0;
    ivars->feedback_completions = 0;
    ivars->feedback_submits = 0;
    ivars->feedback_rearms = 0;
    ivars->feedback_in_flight = false;
    ivars->output_completions = 0;
    // A feedback endpoint reports once every 2^(bInterval - 1) microframes, and the frame
    // list is one entry per report, so 32 entries against this DAC's interval of 4 span 32
    // bus frames rather than the 4 an output transfer uses. Advancing by the output chain's
    // span left every resubmit pointing 28 ms into the past: the first went out, its
    // completion arrived 32 ms later, and everything after it came back kIOReturnIsoTooOld.
    // Take as many entries as cover one output transfer's worth of bus time instead.
    const uint32_t microframes_per_report =
        alt->feedback_interval > 0 && alt->feedback_interval <= 16
            ? 1u << (alt->feedback_interval - 1)
            : 1u;
    ivars->feedback_entries = kMicroframesPerTransfer / microframes_per_report;
    if (ivars->feedback_entries == 0) {
        ivars->feedback_entries = 1;
    }
    ivars->carry = 0.0;
    // Pick the timeline up where it was left, rather than starting it again at zero.
    //
    // Core Audio's sample time carries across an IO stop and start, and its counter follows
    // the timeline the driver posts rather than restarting alongside it: the sample time the
    // host writes at on the first cycle of a track is the number of frames the track before
    // it played. Zeroing the counter here anchors the two to different timelines, so the ring
    // is read nowhere near where the host writes, and the HAL then walks the difference off at
    // a few thousand frames a second -- crossing the write point, and taking the audio with
    // it, every few seconds for the minute or more that takes.
    //
    // The last timestamp posted is where the host is, to within a period, and it is already a
    // multiple of one, so it needs no rounding. Before the first track it reads back zero,
    // which is where this used to start.
    uint64_t resumed_sample = 0;
    uint64_t resumed_host = 0;
    ivars->device->GetCurrentZeroTimestamp(&resumed_sample, &resumed_host);
    // What the host's timeline had reached against where this engine last left off. The two
    // are not the same number and the difference is not elapsed time: it has been measured at
    // roughly a ring whether the engine was down for ten minutes or seventeen. Logged because
    // the read offset below exists to absorb it, and the size of it is the thing to watch.
    Log("resuming at %llu, engine last reached %llu, so the host's timeline is %lld ahead",
        resumed_sample, ivars->sample_counter,
        static_cast<int64_t>(resumed_sample) - static_cast<int64_t>(ivars->sample_counter));
    ivars->sample_counter = resumed_sample;
    ivars->next_timestamp_at = resumed_sample + kZeroTimestampPeriod;
    ivars->prev_sample = 0;
    ivars->prev_host = 0;
    ivars->io_calls = 0;
    ivars->timestamps_posted = 0;
    ivars->crossings = 0;
    ivars->laps = 0;
    ivars->client_active = false;
    ivars->idle_completions = 0;
    ivars->idle_stopping = false;
    ivars->last_write_sample = 0;
    ivars->last_write_frames = 0;
    ivars->read_offset = 0;
    ivars->timeline_anchored = false;
    ivars->starved = 0;
    ivars->silence_byte = alt->raw_data ? kDsdSilenceByte : 0;
    ivars->active_rate = rate;
    ivars->running = true;
    // The ring is read a whole in-flight window before the host's first write of the track
    // lands, so it starts out holding silence rather than the tail of the track before. Which
    // byte means silence depends on the carrier: a DSD DAC fed zeroes leaves lock and pops.
    memset(reinterpret_cast<void*>(ivars->ring_map->GetAddress()), ivars->silence_byte,
           ivars->ring_map->GetLength());

    result = AllocateTransfers(this, ivars, alt->max_packet);
    if (result != kIOReturnSuccess) {
        return result;
    }

    uint64_t bus_frame = 0;
    uint64_t when = 0;
    result = ivars->interface->GetFrameNumber(&bus_frame, &when);
    if (result != kIOReturnSuccess) {
        return result;
    }
    ivars->next_bus_frame = bus_frame + kStartLead;

    for (uint32_t index = 0; index < kTransfersInFlight; index++) {
        result = SubmitTransfer(index);
        if (result != kIOReturnSuccess) {
            return result;
        }
    }
    // After the output chain, not before: `SubmitFeedback` schedules against its queue
    // point, and the frame the loop started on is already behind the bus by the time the
    // last transfer is queued.
    const kern_return_t feedback = SubmitFeedback();
    if (feedback != kIOReturnSuccess) {
        Log("feedback endpoint would not start (0x%08x), running open loop", feedback);
    }
    Log("streaming %u Hz on alt %u: ring %llu frames of %u bytes, timeline resumes at %llu, "
        "feedback endpoint 0x%02x pipe %{public}s, interval %u payload %u, %u entries",
        rate, alt_setting, ivars->ring_frames, ivars->frame_bytes, ivars->sample_counter,
        alt->feedback_endpoint, ivars->feedback_pipe != nullptr ? "open" : "none",
        static_cast<unsigned>(alt->feedback_interval), alt->feedback_max_packet,
        ivars->feedback_entries);
    return kIOReturnSuccess;
}

void DsdAudioDriver::StopIsoc() {
    if (ivars->running) {
        if (ivars->crossings != 0 || ivars->laps != 0 || ivars->starved != 0) {
            Log("stream ends: %llu cycles the engine had overtaken the host, %llu cycles the "
                "host had lapped the read point, %llu frames sent as silence",
                ivars->crossings, ivars->laps, ivars->starved);
            Log("feedback over the session: %llu submits, %llu completions, %llu reports, "
                "%llu misses, %llu re-arms",
                ivars->feedback_submits, ivars->feedback_completions, ivars->feedback_reports,
                ivars->feedback_misses, ivars->feedback_rearms);
        }
        ivars->running = false;
        ivars->idle_stopping = false;
        if (ivars->pipe != nullptr) {
            // Synchronous, so nothing is still holding a buffer when they are freed below.
            ivars->pipe->Abort(kIOUSBAbortSynchronous, kIOReturnAborted, this);
            OSSafeReleaseNULL(ivars->pipe);
        }
        if (ivars->feedback_pipe != nullptr) {
            ivars->feedback_pipe->Abort(kIOUSBAbortSynchronous, kIOReturnAborted, this);
            OSSafeReleaseNULL(ivars->feedback_pipe);
        }
        if (ivars->interface != nullptr && ivars->active_alt != 0) {
            ivars->interface->SelectAlternateSetting(0);
            ivars->active_alt = 0;
        }
    }
    // Unconditional: a start that failed part way through leaves transfers allocated.
    FreeTransfers(ivars);
}

kern_return_t DsdAudioDriver::SubmitTransfer(uint32_t index) {
    if (!ivars->running || ivars->pipe == nullptr || index >= kTransfersInFlight) {
        return kIOReturnNotReady;
    }
    Transfer& transfer = ivars->transfers[index];
    if (transfer.data_map == nullptr || transfer.frame_map == nullptr ||
        !ivars->ring_map || ivars->frame_bytes == 0) {
        return kIOReturnNotReady;
    }
    uint8_t* data = reinterpret_cast<uint8_t*>(transfer.data_map->GetAddress());
    IOUSBIsochronousFrame* frames =
        reinterpret_cast<IOUSBIsochronousFrame*>(transfer.frame_map->GetAddress());
    const uint8_t* ring = reinterpret_cast<const uint8_t*>(ivars->ring_map->GetAddress());
    const uint32_t stride = ivars->frame_bytes;
    const uint32_t packet =
        static_cast<uint32_t>(transfer.data_map->GetLength() / kMicroframesPerTransfer);

    transfer.start_sample = ivars->sample_counter;
    uint32_t offset = 0;
    for (uint32_t microframe = 0; microframe < kMicroframesPerTransfer; microframe++) {
        // Carrying the fraction forward makes a rate that is not a whole number of samples
        // per microframe average out exactly rather than drifting a sample at a time.
        const double wanted = ivars->samples_per_microframe + ivars->carry;
        uint32_t samples = static_cast<uint32_t>(wanted);
        ivars->carry = wanted - static_cast<double>(samples);
        if (samples * stride > packet) {
            samples = packet / stride;
        }
        // Read a fixed lag behind the submission point, and never past what the host has
        // actually written.
        //
        // The lag is the whole geometry and it never moves: the transfer starting at sample
        // S carries the host's audio for index S minus the lag, so what is heard is the lag
        // behind the timeline's own time for that index, which is what the reported latency
        // says.
        //
        // The bound is for the start, where it is not steady. Core Audio writes at a
        // fraction of real time for about a second after IO first begins while the engine
        // runs at rate from the first transfer, and the audio to fill that gap does not
        // exist yet -- no choice of read position conjures it. Silence is the honest thing
        // to send, and the bound stops mattering the moment the host is running at rate.
        const uint64_t at = ivars->ReadPointFor(ivars->sample_counter);
        const uint64_t written_to = ivars->last_write_sample + ivars->last_write_frames;
        if (ivars->last_write_frames == 0 || at + samples > written_to) {
            memset(data + offset, ivars->silence_byte, samples * stride);
            // Silence with no client is the engine idling, not the host failing to keep up,
            // and counting it would bury the number that says how long a start took.
            if (ivars->client_active) {
                ivars->starved += samples;
            }
        } else {
            CopyFromRing(ring, ivars->ring_frames, at, samples, stride, data + offset);
        }
        ivars->sample_counter += samples;

        frames[microframe].status = kIOReturnInvalid;
        frames[microframe].requestCount = samples * stride;
        frames[microframe].completeCount = 0;
        frames[microframe].reserved = 0;
        frames[microframe].timeStamp = 0;
        // The buffer is packed: each microframe starts where the one before it ended, so a
        // frame that carries fewer samples leaves no gap.
        offset += samples * stride;
    }

    // Roughly one report a second: the first handful are pre-roll submitted before the host
    // has written anything, so they say nothing about whether audio is arriving.
    const kern_return_t result = ivars->pipe->IsochIO(transfer.data, transfer.frames,
                                                      ivars->next_bus_frame, transfer.completion);
    if (result != kIOReturnSuccess) {
        Log("cannot submit transfer %u: 0x%08x", index, result);
        return result;
    }
    ivars->next_bus_frame += kMicroframesPerTransfer / kMicroframesPerFrame;
    return kIOReturnSuccess;
}

/// Which transfer an action belongs to, or `kTransfersInFlight` for one that is not ours.
uint32_t TransferForAction(const DsdAudioDriver_IVars* ivars, const OSAction* action) {
    for (uint32_t index = 0; index < kTransfersInFlight; index++) {
        if (ivars->transfers[index].completion == action) {
            return index;
        }
    }
    return kTransfersInFlight;
}

kern_return_t DsdAudioDriver::SubmitFeedback() {
    if (!ivars->running || ivars->feedback_pipe == nullptr ||
        ivars->feedback.frame_map == nullptr || ivars->feedback_entries == 0) {
        return kIOReturnNotReady;
    }
    // Schedule against the output chain's queue point, which is a whole in-flight window
    // into the future and therefore always a frame the controller will still take.
    //
    // A feedback chain has one transfer outstanding, so it has no lead of its own:
    // resubmitting from its own completion aims at the frame that transfer just finished,
    // which has gone by the time the handler runs. Advancing a counter of its own does not
    // help either -- it only moves on success, so the first refusal pins it and every retry
    // afterwards aims at the same receding frame. Both chains take one completion per output
    // transfer, so this neither overlaps the previous submission nor leaves a gap.
    const uint64_t at_frame = ivars->next_bus_frame;
    IOUSBIsochronousFrame* frames =
        reinterpret_cast<IOUSBIsochronousFrame*>(ivars->feedback.frame_map->GetAddress());
    for (uint32_t entry = 0; entry < ivars->feedback_entries; entry++) {
        frames[entry].status = kIOReturnInvalid;
        frames[entry].requestCount = kFeedbackBytes;
        frames[entry].completeCount = 0;
        frames[entry].reserved = 0;
        frames[entry].timeStamp = 0;
    }
    ivars->feedback_submits++;
    const kern_return_t result =
        ivars->feedback_pipe->IsochIO(ivars->feedback.data, ivars->feedback.frames,
                                      at_frame, ivars->feedback.completion);
    if (result != kIOReturnSuccess) {
        // The first few unconditionally. Every symptom of a refused submit is an absence --
        // no report, no miss, no line at all -- which is indistinguishable from an endpoint
        // that was never asked, and that is how this one has looked from the start.
        if (ivars->feedback_submits <= 4) {
            Log("feedback submit %llu refused at bus frame %llu: 0x%08x",
                ivars->feedback_submits, at_frame, result);
        }
        return result;
    }
    ivars->feedback_in_flight = true;
    return kIOReturnSuccess;
}

/// An asynchronous endpoint runs on the DAC's clock, not the host's, and says how many
/// samples it wants per microframe as a 16.16 fixed point count. Sending the nominal count
/// instead walks the DAC's buffer until it breaks, which is audible as a glitch every few
/// seconds.
void HandleFeedback(DsdAudioDriver_IVars* ivars, IOReturn status) {
    ivars->feedback_in_flight = false;
    ivars->feedback_completions++;
    // The first completion entry by entry, so a chain that runs once and stops is
    // distinguishable from one that never ran, and so a report that is being discarded is
    // distinguishable from one that never arrived. Both were silent before: a handful of
    // accepted reports is never a multiple of the summary interval, and a report having been
    // seen at all suppressed the miss line.
    if (ivars->feedback_completions == 1 && ivars->feedback.frame_map != nullptr) {
        const IOUSBIsochronousFrame* first =
            reinterpret_cast<const IOUSBIsochronousFrame*>(ivars->feedback.frame_map->GetAddress());
        for (uint32_t entry = 0; entry < ivars->feedback_entries; entry++) {
            Log("feedback completion 1 entry %u: status 0x%08x count %u (transfer 0x%08x)",
                entry, first[entry].status, first[entry].completeCount, status);
        }
    }
    // Judge each interval by its own status, not by the transfer's.
    //
    // This DAC's feedback transfers come back kIOReturnOverrun every time, while every
    // interval in them is marked success and holds its four bytes -- not some of them, all
    // of them. Whatever the aggregate is reporting, it is not whether the reports arrived,
    // and gating the read on it threw away 475 completions in a row without one line to say
    // so. The per-interval status is the one that answers the question being asked.
    if (ivars->feedback.data_map != nullptr && ivars->feedback.frame_map != nullptr) {
        const uint8_t* data = reinterpret_cast<const uint8_t*>(ivars->feedback.data_map->GetAddress());
        const IOUSBIsochronousFrame* frames =
            reinterpret_cast<const IOUSBIsochronousFrame*>(ivars->feedback.frame_map->GetAddress());
        // Only the entries actually submitted: the rest of the list still holds the counts
        // the transfer before it left there, and reading those counts a report twice.
        for (uint32_t entry = 0; entry < ivars->feedback_entries; entry++) {
            const bool carried = frames[entry].status == kIOReturnSuccess ||
                                 frames[entry].status == kIOReturnUnderrun;
            if (!carried || frames[entry].completeCount < kFeedbackBytes) {
                continue;
            }
            const uint8_t* report = data + entry * kFeedbackBytes;
            const uint32_t raw = static_cast<uint32_t>(report[0]) |
                                 (static_cast<uint32_t>(report[1]) << 8) |
                                 (static_cast<uint32_t>(report[2]) << 16) |
                                 (static_cast<uint32_t>(report[3]) << 24);
            const double samples = static_cast<double>(raw) / 65536.0;
            const double ratio = samples / ivars->nominal_samples_per_microframe;
            // Anything outside the band is a decoding error, not a clock.
            if (ratio >= kMinFeedbackRatio && ratio <= kMaxFeedbackRatio) {
                ivars->samples_per_microframe = samples;
                ivars->feedback_reports++;
                if (ivars->feedback_reports == 1 || ivars->feedback_reports % 2000 == 0) {
                    Log("DAC asks for %d.%06d samples per microframe, nominal %d.%06d",
                        static_cast<int>(samples),
                        static_cast<int>((samples - static_cast<int>(samples)) * 1000000),
                        static_cast<int>(ivars->nominal_samples_per_microframe),
                        static_cast<int>((ivars->nominal_samples_per_microframe -
                                          static_cast<int>(ivars->nominal_samples_per_microframe)) *
                                         1000000));
                }
            }
        }
    }
    if (ivars->feedback_reports == 0) {
        ivars->feedback_misses++;
        if (ivars->feedback_misses % 200 == 1) {
            const IOUSBIsochronousFrame* frames =
                ivars->feedback.frame_map != nullptr
                    ? reinterpret_cast<const IOUSBIsochronousFrame*>(
                          ivars->feedback.frame_map->GetAddress())
                    : nullptr;
            Log("feedback came back 0x%08x, first frame status 0x%08x count %u", status,
                frames != nullptr ? frames[0].status : 0,
                frames != nullptr ? frames[0].completeCount : 0);
        }
    }
}

void DsdAudioDriver::ScheduleIdleTeardown() {
    // Not from here: `StopIsoc` aborts the pipe synchronously and then frees the buffers and
    // the completion action this handler is running on. It has to happen once the handler
    // has returned, which is what the queue is for.
    IODispatchQueue* queue = nullptr;
    if (CopyDispatchQueue(kIOServiceDefaultQueueName, &queue) != kIOReturnSuccess ||
        queue == nullptr) {
        // Leave the engine up rather than tear it down from the wrong context, and let the
        // count run again so this is retried rather than given up on.
        Log("no dispatch queue to retire the idle engine on, leaving it streaming");
        ivars->idle_stopping = false;
        ivars->idle_completions = 0;
        return;
    }
    // The block outlives this call and reaches for ivars, which `free` deletes.
    retain();
    queue->DispatchAsync(^{
        // A client may have arrived in between, in which case `StopIsoc` has already run
        // from StartDevice and cleared this, and the engine is wanted again.
        if (ivars->idle_stopping) {
            Log("engine idle for %llu seconds with no client, dropping to alt 0",
                kIdleTeardownMs / 1000);
            StopIsoc();
        }
        release();
    });
    OSSafeReleaseNULL(queue);
}

void IMPL(DsdAudioDriver, IsochComplete) {
    if (ivars == nullptr || !ivars->running || action == nullptr) {
        return;
    }
    if (action == ivars->feedback.completion) {
        HandleFeedback(ivars, status);
        // Always resubmit: a servo that stops silently stops tracking the DAC's clock. The
        // one exception is a teardown already scheduled, where resubmitting only gives the
        // abort more to reclaim.
        if (!ivars->idle_stopping) {
            SubmitFeedback();
        }
        return;
    }
    const uint32_t index = TransferForAction(ivars, action);
    if (index >= kTransfersInFlight) {
        return;
    }
    if (status != kIOReturnSuccess && status != kIOReturnUnderrun) {
        Log("transfer %u came back 0x%08x, stopping", index, status);
        return;
    }
    // How long the engine has been running with nobody listening. A completion is a fixed
    // slice of bus time, so this counts seconds without a timer, and the handler that has to
    // read it is already running at every one.
    if (ivars->client_active) {
        ivars->idle_completions = 0;
    } else if (!ivars->idle_stopping && ++ivars->idle_completions >= kIdleTeardownCompletions) {
        ivars->idle_stopping = true;
        ScheduleIdleTeardown();
    }
    if (ivars->idle_stopping) {
        // Draining. Nothing is resubmitted, so the abort the teardown runs has less to
        // reclaim, and the timeline stops advancing because no client is reading it.
        return;
    }
    const IOUSBIsochronousFrame* transfer_frames_for_log =
        ivars->transfers[index].frame_map != nullptr
            ? reinterpret_cast<const IOUSBIsochronousFrame*>(
                  ivars->transfers[index].frame_map->GetAddress())
            : nullptr;

    ivars->output_completions++;
    if (ivars->output_completions <= 3 && transfer_frames_for_log != nullptr) {
        Log("completion %llu: status 0x%08x, frame 0 status 0x%08x count %u, engine at %llu",
            ivars->output_completions, status, transfer_frames_for_log[0].status,
            transfer_frames_for_log[0].completeCount, ivars->sample_counter);
    }
    // A feedback chain that stops takes the servo down with it silently, because the chain
    // only resubmits from its own completion. It stops for more reasons than never starting:
    // one refused submit ends it just as completely. So re-arm on whether a read is
    // outstanding rather than on whether a report was ever heard.
    if (ivars->feedback_pipe != nullptr && !ivars->feedback_in_flight &&
        ivars->output_completions % 250 == 0) {
        const kern_return_t armed = SubmitFeedback();
        if (ivars->feedback_rearms < 4) {
            Log("feedback chain idle after %llu transfers, re-arm says 0x%08x",
                ivars->output_completions, armed);
        }
        ivars->feedback_rearms++;
    }

    // The DAC's own clock decides when a frame goes out, so timing the host's timeline off
    // these completions is what removes drift: Core Audio follows the DAC rather than the
    // two running open loop against each other.
    //
    // The sample times have to land on exact multiples of the zero timestamp period, which
    // is what the device promised the host when it was created. A transfer carries
    // rate/250 samples, which is not a whole number at most rates and never divides the
    // period, so the boundary falls inside a transfer and its host time is interpolated
    // between this completion and the one before. Posting the transfer's own sample time
    // instead makes Core Audio reject every timestamp as TimeStampOutOfLine, reset its
    // clock, and never advance where it writes.
    const Transfer& transfer = ivars->transfers[index];
    if (transfer.frame_map != nullptr) {
        const IOUSBIsochronousFrame* frames =
            reinterpret_cast<const IOUSBIsochronousFrame*>(transfer.frame_map->GetAddress());
        const uint64_t sample = transfer.start_sample;
        const uint64_t host = frames[0].timeStamp;
        // Give the host a pair from this session as soon as there is one to give.
        //
        // Nothing is posted while IO is stopped, so until the first period boundary lands
        // the newest pair the host holds is the last one of the previous session, and it
        // works out where to write from that. A period is 372 ms at 44100. Sessions where
        // the host began writing on that first boundary opened tens of thousands of frames
        // behind the engine and spent a second converging on it; the ones that happened to
        // wait for the second boundary opened at rate and needed no convergence at all.
        // This is the same pair, exact -- the sample the first transfer starts at, against
        // the bus time it actually went out on -- roughly fourteen milliseconds in.
        if (ivars->prev_host == 0 && ivars->device) {
            ivars->device->UpdateCurrentZeroTimestamp(sample, host);
            Log("seeded the timeline at sample %llu, host time %llu", sample, host);
        }
        if (ivars->prev_host != 0 && sample > ivars->prev_sample && ivars->device) {
            const double per_sample = static_cast<double>(host - ivars->prev_host) /
                                      static_cast<double>(sample - ivars->prev_sample);
            while (ivars->next_timestamp_at <= sample &&
                   ivars->next_timestamp_at >= ivars->prev_sample) {
                const uint64_t at = ivars->next_timestamp_at;
                const uint64_t when =
                    ivars->prev_host +
                    static_cast<uint64_t>(static_cast<double>(at - ivars->prev_sample) *
                                          per_sample);
                ivars->device->UpdateCurrentZeroTimestamp(at, when);
                if (ivars->timestamps_posted < 3) {
                    Log("posted zero timestamp %llu at host time %llu", at, when);
                }
                ivars->timestamps_posted++;
                ivars->next_timestamp_at += kZeroTimestampPeriod;
            }
        }
        ivars->prev_sample = sample;
        ivars->prev_host = host;
    }
    SubmitTransfer(index);
}
