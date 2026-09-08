//
//  probe.cpp
//  Runs the dext's UAC2 parser over an attached device, on the host.
//
//  The dext cannot be loaded without entitlements Apple grants case by case, so this is how
//  the parser is held to real hardware in the meantime: it reads the same configuration
//  descriptor and the same clock RANGE report the dext reads, through the IOKit API the CLI
//  uses, and prints what DsdUac2.cpp makes of them.
//
//  Reading either needs no claim on the device. usbaudiod holds the streaming interfaces,
//  not the device, so opening the device for one control transfer races nothing.
//
//  Build and run with ../build.sh probe.
//

#include <CoreFoundation/CoreFoundation.h>
#include <IOKit/IOCFPlugIn.h>
#include <IOKit/IOKitLib.h>
#include <IOKit/usb/IOUSBLib.h>

#include <cstdio>

#include "DsdUac2.h"

namespace {

/// UAC2 class request reading the range of the sampling frequency control.
constexpr uint8_t kRequestTypeGetInterface = 0xA1;
constexpr uint8_t kUac2RangeCur = 0x02;
constexpr uint16_t kUac2SamplingFreqControl = 0x0100;

IOUSBDeviceInterface500** CopyDeviceInterface(io_service_t service) {
    IOCFPlugInInterface** plugin = nullptr;
    SInt32 score = 0;
    if (IOCreatePlugInInterfaceForService(service, kIOUSBDeviceUserClientTypeID,
                                          kIOCFPlugInInterfaceID, &plugin,
                                          &score) != kIOReturnSuccess ||
        plugin == nullptr) {
        return nullptr;
    }
    IOUSBDeviceInterface500** device = nullptr;
    (*plugin)->QueryInterface(plugin, CFUUIDGetUUIDBytes(kIOUSBDeviceInterfaceID500),
                              reinterpret_cast<void**>(&device));
    IODestroyPlugInInterface(plugin);
    return device;
}

/// Ask the clock source for its range, and parse it. Returns the number of rates written.
size_t ReadClockRates(IOUSBDeviceInterface500** device, const dsd::Uac2Layout& layout,
                      uint32_t* rates, size_t capacity) {
    if ((*device)->USBDeviceOpen(device) != kIOReturnSuccess) {
        std::printf("  cannot open the device to read its clock range\n");
        return 0;
    }
    uint8_t report[2 + 12 * dsd::kMaxSampleRates] = {};
    IOUSBDevRequest request{};
    request.bmRequestType = kRequestTypeGetInterface;
    request.bRequest = kUac2RangeCur;
    request.wValue = kUac2SamplingFreqControl;
    request.wIndex =
        static_cast<UInt16>((layout.clock_id << 8) | layout.control_interface);
    request.wLength = sizeof(report);
    request.pData = report;

    size_t count = 0;
    if ((*device)->DeviceRequest(device, &request) == kIOReturnSuccess) {
        // The device answers with the whole buffer whatever it has to say, so the subrange
        // count at the head is what bounds the report, not the byte count returned.
        count = dsd::ParseClockRange(report, request.wLenDone, rates, capacity);
    } else {
        std::printf("  clock RANGE request failed\n");
    }
    (*device)->USBDeviceClose(device);
    return count;
}

void ReportDevice(IOUSBDeviceInterface500** device) {
    IOUSBConfigurationDescriptorPtr descriptor = nullptr;
    if ((*device)->GetConfigurationDescriptorPtr(device, 0, &descriptor) != kIOReturnSuccess ||
        descriptor == nullptr) {
        return;
    }
    const uint8_t* bytes = reinterpret_cast<const uint8_t*>(descriptor);
    const size_t total = static_cast<size_t>(bytes[2]) | (static_cast<size_t>(bytes[3]) << 8);

    dsd::Uac2Layout layout{};
    if (!dsd::ParseLayout(bytes, total, &layout)) {
        return;
    }

    UInt16 vendor = 0;
    UInt16 product = 0;
    (*device)->GetDeviceVendor(device, &vendor);
    (*device)->GetDeviceProduct(device, &product);
    // Decimal too, because that is what an Info.plist personality wants.
    std::printf("device %04x:%04x  (decimal %u:%u, %zu byte configuration descriptor)\n", vendor,
                product, vendor, product, total);
    std::printf("  control interface %u, streaming interface %u, clock id %u\n",
                layout.control_interface, layout.streaming_interface, layout.clock_id);
    for (size_t index = 0; index < layout.alt_count; index++) {
        const dsd::AltSetting& alt = layout.alts[index];
        std::printf("  alt %u  %s  %u ch  %u byte subslot  %u bits  ep 0x%02x  fb 0x%02x  "
                    "maxpkt %u  interval %u\n",
                    alt.alt_setting, alt.raw_data ? "RAW_DATA" : "PCM     ", alt.channels,
                    alt.subslot_bytes, alt.bit_resolution, alt.out_endpoint,
                    alt.feedback_endpoint, alt.max_packet, alt.interval);
    }

    uint32_t rates[dsd::kMaxSampleRates] = {};
    const size_t rate_count = ReadClockRates(device, layout, rates, dsd::kMaxSampleRates);
    std::printf("  %zu rates from the clock:", rate_count);
    for (size_t index = 0; index < rate_count; index++) {
        std::printf(" %u", rates[index]);
    }
    std::printf("\n");

    dsd::FormatEntry formats[dsd::kMaxAltSettings * dsd::kMaxSampleRates];
    constexpr size_t kCapacity = sizeof(formats) / sizeof(formats[0]);
    const size_t count = dsd::BuildFormats(layout, rates, rate_count, formats, kCapacity);
    std::printf("  %zu formats the dext would publish:\n", count);
    for (size_t index = 0; index < count; index++) {
        const dsd::FormatEntry& entry = formats[index];
        std::printf("    %8.0f Hz  alt %u  %s  %u ch  %u bits in %u bytes\n", entry.sample_rate,
                    entry.alt_setting, entry.native_dsd ? "native DSD" : "PCM       ",
                    entry.channels, entry.bit_resolution, entry.subslot_bytes);
    }
}

}  // namespace

int main() {
    io_iterator_t iterator = 0;
    if (IOServiceGetMatchingServices(kIOMainPortDefault, IOServiceMatching("IOUSBHostDevice"),
                                     &iterator) != kIOReturnSuccess) {
        std::printf("cannot enumerate USB devices\n");
        return 1;
    }
    for (io_service_t service = IOIteratorNext(iterator); service != 0;
         service = IOIteratorNext(iterator)) {
        IOUSBDeviceInterface500** device = CopyDeviceInterface(service);
        if (device != nullptr) {
            ReportDevice(device);
            (*device)->Release(device);
        }
        IOObjectRelease(service);
    }
    IOObjectRelease(iterator);
    return 0;
}
