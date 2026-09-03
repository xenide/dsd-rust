//
//  DsdAudioDriver.cpp
//

#include <DsdAudioDriver/DsdAudioDriver.h>

#include <AudioDriverKit/AudioDriverKit.h>
#include <DriverKit/DriverKit.h>
#include <DriverKit/IOBufferMemoryDescriptor.h>
#include <DriverKit/IOLib.h>
#include <DriverKit/IOMemoryMap.h>
#include <DriverKit/OSSharedPtr.h>
#include <USBDriverKit/USBDriverKit.h>

#include "DsdUac2.h"

#define Log(fmt, ...) os_log(OS_LOG_DEFAULT, "DsdAudioDriver: " fmt, ##__VA_ARGS__)

namespace {

/// Microframes one transfer carries, so four milliseconds of audio.
constexpr uint32_t kMicroframesPerTransfer = 32;
/// Transfers in flight, giving thirty-two milliseconds of scheduling headroom.
constexpr uint32_t kTransfersInFlight = 8;
/// High-speed USB divides each one millisecond bus frame into eight microframes.
constexpr uint64_t kMicroframesPerFrame = 8;
/// Bus frames to schedule the first transfer ahead of now.
constexpr uint64_t kStartLead = 10;
/// Sample frames the ring holds. Large enough for the biggest IO buffer the host asks for
/// at the highest rate, and a power of two so the wrap is a mask.
constexpr uint32_t kRingFrames = 32768;
/// Sample frames between the timestamps the host reads to build its timeline.
constexpr uint32_t kZeroTimestampPeriod = 4096;

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

    OSSharedPtr<IOUserAudioDevice> device;
    OSSharedPtr<IOUserAudioStream> stream;
    OSSharedPtr<IOBufferMemoryDescriptor> ring;
    OSSharedPtr<IOMemoryMap> ring_map;

    dsd::Uac2Layout layout;
    dsd::FormatEntry formats[dsd::kMaxAltSettings * dsd::kMaxSampleRates];
    size_t format_count;

    Transfer transfers[kTransfersInFlight];
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
    bool running;
    uint8_t active_alt;
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
IOUserAudioStreamBasicDescription Describe(const dsd::FormatEntry& entry) {
    IOUserAudioStreamBasicDescription format{};
    format.mSampleRate = entry.sample_rate;
    format.mFormatID = IOUserAudioFormatID::LinearPCM;

    uint32_t flags = IOUserAudioFormatFlags::FormatFlagIsSignedInteger;
    if (entry.native_dsd) {
        flags |= IOUserAudioFormatFlags::FormatFlagIsBigEndian |
                 IOUserAudioFormatFlags::FormatFlagIsNonMixable;
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

/// Widest sample frame any published format uses, which is what the ring must hold.
uint32_t WidestFrameBytes(const DsdAudioDriver_IVars* ivars) {
    uint32_t widest = 0;
    for (size_t index = 0; index < ivars->format_count; index++) {
        const dsd::FormatEntry& entry = ivars->formats[index];
        const uint32_t bytes = static_cast<uint32_t>(entry.channels) * entry.subslot_bytes;
        if (bytes > widest) {
            widest = bytes;
        }
    }
    return widest;
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

    const uint32_t widest = WidestFrameBytes(ivars);
    IOBufferMemoryDescriptor* ring = nullptr;
    kern_return_t result = IOBufferMemoryDescriptor::Create(
        kIOMemoryDirectionInOut, static_cast<uint64_t>(kRingFrames) * widest, 4096, &ring);
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

    IOUserAudioStreamBasicDescription formats[dsd::kMaxAltSettings * dsd::kMaxSampleRates];
    double rates[dsd::kMaxSampleRates];
    size_t rate_count = 0;
    for (size_t index = 0; index < ivars->format_count; index++) {
        formats[index] = Describe(ivars->formats[index]);
        bool seen = false;
        for (size_t known = 0; known < rate_count; known++) {
            seen = seen || rates[known] == formats[index].mSampleRate;
        }
        if (!seen && rate_count < dsd::kMaxSampleRates) {
            rates[rate_count++] = formats[index].mSampleRate;
        }
    }
    ivars->stream->SetAvailableStreamFormats(formats, static_cast<uint32_t>(ivars->format_count));
    ivars->stream->SetCurrentStreamFormat(&formats[0]);
    ivars->device->SetAvailableSampleRates(rates, rate_count);
    ivars->device->SetSampleRate(formats[0].mSampleRate);

    result = ivars->device->AddStream(ivars->stream.get());
    if (result != kIOReturnSuccess) {
        Log("AddStream failed: 0x%08x", result);
        return result;
    }
    // Output IO needs no work here: the host writes into the ring the stream was created
    // over, and the isochronous engine reads it at the position its own timeline names.
    ivars->device->SetIOOperationHandler(
        ^kern_return_t(IOUserAudioObjectID, IOUserAudioIOOperation, uint32_t, uint64_t, uint64_t) {
            return kIOReturnSuccess;
        });
    result = AddObject(ivars->device.get());
    if (result != kIOReturnSuccess) {
        Log("AddObject failed: 0x%08x", result);
    }
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

void FreeTransfers(DsdAudioDriver_IVars* ivars) {
    for (uint32_t index = 0; index < kTransfersInFlight; index++) {
        Transfer& transfer = ivars->transfers[index];
        OSSafeReleaseNULL(transfer.completion);
        OSSafeReleaseNULL(transfer.data_map);
        OSSafeReleaseNULL(transfer.frame_map);
        OSSafeReleaseNULL(transfer.data);
        OSSafeReleaseNULL(transfer.frames);
    }
}

/// Give every transfer its own data buffer, frame list and completion action.
///
/// Each buffer is sized for the endpoint's largest packet in every microframe it covers, so
/// a rate that is not a whole number of samples per microframe still fits its long frames.
kern_return_t AllocateTransfers(DsdAudioDriver* driver, DsdAudioDriver_IVars* ivars,
                                uint32_t max_packet) {
    FreeTransfers(ivars);
    const uint64_t data_bytes = static_cast<uint64_t>(kMicroframesPerTransfer) * max_packet;
    const uint64_t frame_bytes =
        static_cast<uint64_t>(kMicroframesPerTransfer) * sizeof(IOUSBIsochronousFrame);

    for (uint32_t index = 0; index < kTransfersInFlight; index++) {
        Transfer& transfer = ivars->transfers[index];
        kern_return_t result = IOBufferMemoryDescriptor::Create(kIOMemoryDirectionOut, data_bytes,
                                                                4096, &transfer.data);
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
        result = transfer.frames->CreateMapping(0, 0, 0, 0, 0, &transfer.frame_map);
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
    return kIOReturnSuccess;
}

}  // namespace

kern_return_t DsdAudioDriver::StartDevice(IOUserAudioObjectID in_object_id,
                                          IOUserAudioStartStopFlags in_flags) {
    const IOUserAudioStreamBasicDescription format = ivars->stream->GetCurrentStreamFormat();
    const dsd::FormatEntry* entry = nullptr;
    for (size_t index = 0; index < ivars->format_count; index++) {
        const dsd::FormatEntry& candidate = ivars->formats[index];
        const uint32_t frame_bytes =
            static_cast<uint32_t>(candidate.channels) * candidate.subslot_bytes;
        if (candidate.sample_rate == format.mSampleRate && frame_bytes == format.mBytesPerFrame &&
            candidate.bit_resolution == format.mBitsPerChannel) {
            entry = &candidate;
            break;
        }
    }
    if (entry == nullptr) {
        Log("no alternate setting carries the format the host asked for");
        return kIOReturnUnsupported;
    }

    const uint32_t frame_bytes = static_cast<uint32_t>(entry->channels) * entry->subslot_bytes;
    const kern_return_t result = StartIsoc(static_cast<uint32_t>(entry->sample_rate),
                                           entry->alt_setting, frame_bytes);
    if (result != kIOReturnSuccess) {
        StopIsoc();
        return result;
    }
    return super::StartDevice(in_object_id, in_flags);
}

kern_return_t DsdAudioDriver::StopDevice(IOUserAudioObjectID in_object_id,
                                         IOUserAudioStartStopFlags in_flags) {
    StopIsoc();
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

    ivars->frame_bytes = frame_bytes;
    ivars->ring_frames = ivars->ring_map->GetLength() / frame_bytes;
    ivars->samples_per_microframe = static_cast<double>(rate) / 8000.0;
    ivars->carry = 0.0;
    ivars->sample_counter = 0;
    ivars->next_timestamp_at = kZeroTimestampPeriod;
    ivars->running = true;
    // A DSD DAC fed zeroes leaves lock and pops, so the ring starts out holding DSD silence
    // rather than what an untouched buffer holds. Anything the host writes replaces it.
    if (alt->raw_data) {
        memset(reinterpret_cast<void*>(ivars->ring_map->GetAddress()), kDsdSilenceByte,
               ivars->ring_map->GetLength());
    }

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
    Log("streaming %u Hz on alternate setting %u", rate, alt_setting);
    return kIOReturnSuccess;
}

void DsdAudioDriver::StopIsoc() {
    if (ivars->running) {
        ivars->running = false;
        if (ivars->pipe != nullptr) {
            // Synchronous, so nothing is still holding a buffer when they are freed below.
            ivars->pipe->Abort(kIOUSBAbortSynchronous, kIOReturnAborted, this);
            OSSafeReleaseNULL(ivars->pipe);
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
        CopyFromRing(ring, ivars->ring_frames, ivars->sample_counter, samples, stride,
                     data + offset);
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

void IMPL(DsdAudioDriver, IsochComplete) {
    if (ivars == nullptr || !ivars->running || action == nullptr) {
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

    // The DAC's own clock decides when a frame goes out, so timing the host's timeline off
    // these completions is what removes drift: Core Audio follows the DAC rather than the
    // two running open loop against each other.
    const Transfer& transfer = ivars->transfers[index];
    if (transfer.start_sample >= ivars->next_timestamp_at && ivars->device &&
        transfer.frame_map != nullptr) {
        const IOUSBIsochronousFrame* frames =
            reinterpret_cast<const IOUSBIsochronousFrame*>(transfer.frame_map->GetAddress());
        ivars->device->UpdateCurrentZeroTimestamp(transfer.start_sample, frames[0].timeStamp);
        ivars->next_timestamp_at = transfer.start_sample + kZeroTimestampPeriod;
    }
    SubmitTransfer(index);
}
