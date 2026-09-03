//
//  DsdUac2.cpp
//

#include "DsdUac2.h"

namespace dsd {
namespace {

constexpr uint8_t kDescInterface = 0x04;
constexpr uint8_t kDescEndpoint = 0x05;
constexpr uint8_t kCsInterface = 0x24;

constexpr uint8_t kAudioClass = 0x01;
constexpr uint8_t kSubclassControl = 0x01;
constexpr uint8_t kSubclassStreaming = 0x02;

constexpr uint8_t kAcClockSource = 0x0A;
constexpr uint8_t kAsGeneral = 0x01;
constexpr uint8_t kAsFormatType = 0x02;

constexpr uint32_t kFormatTypeIRawData = 1u << 31;
constexpr uint8_t kClockFreqHostProgrammable = 0x03;

constexpr uint8_t kTransferTypeMask = 0x03;
constexpr uint8_t kTransferIsochronous = 0x01;
constexpr uint8_t kEndpointDirectionIn = 0x80;

/// High-speed microframes in one second.
constexpr uint32_t kMicroframesPerSecond = 8000;

/// wMaxPacketSize bits 10:0 are the payload; bits 12:11 are additional transactions per
/// microframe, so a device asking for two transactions carries twice the payload.
uint32_t PacketPayload(uint16_t max_packet_size) {
    const uint32_t payload = max_packet_size & 0x07FF;
    const uint32_t additional = (max_packet_size >> 11) & 0x03;
    return payload * (1 + additional);
}

uint32_t ReadLE32(const uint8_t* bytes) {
    return static_cast<uint32_t>(bytes[0]) | (static_cast<uint32_t>(bytes[1]) << 8) |
           (static_cast<uint32_t>(bytes[2]) << 16) | (static_cast<uint32_t>(bytes[3]) << 24);
}

uint16_t ReadLE16(const uint8_t* bytes) {
    return static_cast<uint16_t>(static_cast<uint16_t>(bytes[0]) |
                                 (static_cast<uint16_t>(bytes[1]) << 8));
}

/// An alternate setting is worth publishing once it names a channel count, a subslot width
/// and an output endpoint. A zero channel count would also divide by zero in the rate check.
bool IsPublishable(const AltSetting& alt) {
    return alt.channels > 0 && alt.subslot_bytes > 0 && alt.out_endpoint != 0 &&
           alt.max_packet > 0;
}

/// Native DSD carries four-byte subslots of opaque data. A 32-bit PCM setting has the same
/// subslot width, so the RAW_DATA flag is what separates them.
bool IsNativeDsd(const AltSetting& alt) {
    return alt.raw_data && alt.subslot_bytes == kNativeSubslotBytes;
}

}  // namespace

bool AltCarriesRate(const AltSetting& alt, uint32_t rate) {
    if (rate == 0 || alt.channels == 0 || alt.subslot_bytes == 0) {
        return false;
    }
    // A rate that is not a whole number of samples per microframe still has to fit its
    // largest microframe, which is the one that carries the rounded-up count.
    const uint32_t samples = (rate + kMicroframesPerSecond - 1) / kMicroframesPerSecond;
    const uint32_t bytes = samples * alt.channels * alt.subslot_bytes;
    return bytes <= alt.max_packet;
}

size_t ParseClockRange(const uint8_t* report, size_t length, uint32_t* out, size_t capacity) {
    constexpr size_t kSubrangeBytes = 12;
    if (report == nullptr || length < 2) {
        return 0;
    }
    const size_t subranges = ReadLE16(report);
    size_t written = 0;
    for (size_t index = 0; index < subranges; index++) {
        const size_t offset = 2 + index * kSubrangeBytes;
        if (offset + kSubrangeBytes > length) {
            break;
        }
        const uint32_t min = ReadLE32(report + offset);
        const uint32_t max = ReadLE32(report + offset + 4);
        const uint32_t resolution = ReadLE32(report + offset + 8);
        if (resolution == 0 || min == max) {
            if (written < capacity) {
                out[written++] = min;
            }
            continue;
        }
        for (uint32_t rate = min; rate <= max && written < capacity; rate += resolution) {
            out[written++] = rate;
        }
    }
    return written;
}

size_t BuildFormats(const Uac2Layout& layout, const uint32_t* rates, size_t rate_count,
                    FormatEntry* out, size_t capacity) {
    size_t written = 0;
    // Two passes so every PCM format precedes every native DSD one. Core Audio has no way
    // to say "this is DSD": native goes out as big endian non-mixable integer PCM, which is
    // exactly the shape an application hunting for a bit-perfect PCM format will choose. Put
    // native first and such an application picks it and writes PCM into a DSD stream, which
    // the DAC renders as ticks. Last, it is still there for anything that knows to look.
    for (int native_pass = 0; native_pass <= 1; native_pass++) {
        for (size_t index = 0; index < layout.alt_count; index++) {
            const AltSetting& alt = layout.alts[index];
            const bool native = IsNativeDsd(alt);
            if (native != (native_pass == 1)) {
                continue;
            }
            for (size_t rate_index = 0; rate_index < rate_count && written < capacity;
                 rate_index++) {
                const uint32_t rate = rates[rate_index];
                if (!AltCarriesRate(alt, rate)) {
                    continue;
                }
                FormatEntry& entry = out[written++];
                entry.sample_rate = static_cast<double>(rate);
                entry.alt_setting = alt.alt_setting;
                entry.channels = alt.channels;
                entry.bit_resolution = alt.bit_resolution;
                entry.subslot_bytes = alt.subslot_bytes;
                entry.native_dsd = native;
            }
        }
    }
    return written;
}

namespace {

/// One TLV in the configuration blob.
struct Descriptor {
    uint8_t kind;
    const uint8_t* body;
    size_t body_length;
};

/// Walks a configuration descriptor, stopping at the first malformed length rather than
/// guessing, so a truncated blob cannot be read past its end.
class Walker {
public:
    Walker(const uint8_t* config, size_t length) : config_(config), length_(length) {}

    bool Next(Descriptor* out) {
        if (offset_ + 2 > length_) {
            return false;
        }
        const size_t size = config_[offset_];
        if (size < 2 || offset_ + size > length_) {
            return false;
        }
        out->kind = config_[offset_ + 1];
        out->body = config_ + offset_ + 2;
        out->body_length = size - 2;
        offset_ += size;
        return true;
    }

private:
    const uint8_t* config_;
    size_t length_;
    size_t offset_ = 0;
};

/// What an interface descriptor says about itself, less the fields never read.
struct InterfaceHeader {
    uint8_t number;
    uint8_t alt_setting;
    uint8_t klass;
    uint8_t subclass;
    bool valid;

    bool IsAudioControl() const { return valid && klass == kAudioClass && subclass == kSubclassControl; }
    bool IsAudioStreaming() const {
        return valid && klass == kAudioClass && subclass == kSubclassStreaming;
    }
};

InterfaceHeader ParseInterface(const Descriptor& descriptor) {
    // bInterfaceNumber, bAlternateSetting, bNumEndpoints, bInterfaceClass, bInterfaceSubClass
    if (descriptor.body_length < 5) {
        return InterfaceHeader{};
    }
    return InterfaceHeader{descriptor.body[0], descriptor.body[1], descriptor.body[3],
                           descriptor.body[4], true};
}

void ReadStreamingDescriptor(uint8_t subtype, const uint8_t* fields, size_t length,
                             AltSetting* alt) {
    switch (subtype) {
        // bTerminalLink, bmControls, bFormatType, bmFormats(4), bNrChannels
        case kAsGeneral:
            if (length >= 8) {
                alt->raw_data = (ReadLE32(fields + 3) & kFormatTypeIRawData) != 0;
                alt->channels = fields[7];
            }
            break;
        // bFormatType, bSubslotSize, bBitResolution
        case kAsFormatType:
            if (length >= 3) {
                alt->subslot_bytes = fields[1];
                alt->bit_resolution = fields[2];
            }
            break;
        default:
            break;
    }
}

void ReadEndpoint(const Descriptor& descriptor, AltSetting* alt) {
    // bEndpointAddress, bmAttributes, wMaxPacketSize(2), bInterval
    if (descriptor.body_length < 5) {
        return;
    }
    if ((descriptor.body[1] & kTransferTypeMask) != kTransferIsochronous) {
        return;
    }
    const uint8_t address = descriptor.body[0];
    if ((address & kEndpointDirectionIn) != 0) {
        alt->feedback_endpoint = address;
        return;
    }
    alt->out_endpoint = address;
    alt->max_packet = PacketPayload(ReadLE16(descriptor.body + 2));
    alt->interval = descriptor.body[4];
}

/// Keep an alternate setting once its descriptors agree, and only for the streaming
/// interface the layout has settled on: a device with two streaming interfaces would
/// otherwise mix alternate settings that live on different endpoints.
void CommitAlt(const InterfaceHeader& current, const AltSetting& candidate, Uac2Layout* out) {
    if (!current.IsAudioStreaming() || current.number != out->streaming_interface) {
        return;
    }
    if (!IsPublishable(candidate) || out->alt_count >= kMaxAltSettings) {
        return;
    }
    out->alts[out->alt_count++] = candidate;
}

}  // namespace

bool ParseLayout(const uint8_t* config, size_t length, Uac2Layout* out) {
    if (config == nullptr || out == nullptr) {
        return false;
    }
    *out = Uac2Layout{};

    bool have_control = false;
    bool have_streaming = false;
    uint8_t programmable_clock = 0;
    uint8_t any_clock = 0;
    InterfaceHeader current{};
    AltSetting candidate{};

    Walker walker(config, length);
    Descriptor descriptor{};
    while (walker.Next(&descriptor)) {
        if (descriptor.kind == kDescInterface) {
            CommitAlt(current, candidate, out);
            current = ParseInterface(descriptor);
            candidate = AltSetting{};
            if (current.IsAudioControl() && !have_control) {
                out->control_interface = current.number;
                have_control = true;
            }
            if (current.IsAudioStreaming() && !have_streaming) {
                out->streaming_interface = current.number;
                have_streaming = true;
            }
            candidate.alt_setting = current.alt_setting;
            continue;
        }
        if (descriptor.kind == kCsInterface && descriptor.body_length >= 1) {
            const uint8_t subtype = descriptor.body[0];
            const uint8_t* fields = descriptor.body + 1;
            const size_t fields_length = descriptor.body_length - 1;
            // bClockID, bmAttributes, bmControls
            if (current.IsAudioControl() && subtype == kAcClockSource && fields_length >= 3) {
                if (any_clock == 0) {
                    any_clock = fields[0];
                }
                const bool programmable =
                    (fields[2] & kClockFreqHostProgrammable) == kClockFreqHostProgrammable;
                if (programmable && programmable_clock == 0) {
                    programmable_clock = fields[0];
                }
            } else if (current.IsAudioStreaming()) {
                ReadStreamingDescriptor(subtype, fields, fields_length, &candidate);
            }
            continue;
        }
        if (descriptor.kind == kDescEndpoint) {
            ReadEndpoint(descriptor, &candidate);
        }
    }
    CommitAlt(current, candidate, out);

    out->clock_id = programmable_clock != 0 ? programmable_clock : any_clock;
    return have_control && have_streaming && out->clock_id != 0 && out->alt_count > 0;
}

}  // namespace dsd
