//
//  test_dsd_uac2.cpp
//  Host-side tests for the descriptor parser the dext shares.
//
//  These build and run without Xcode, DriverKit, entitlements or a DAC, which is the whole
//  reason the parser has no DriverKit dependency. Build with driverkit/build.sh test.
//

#include "DsdAudioDriver/DsdUac2.h"

#include <cstdio>
#include <cstring>
#include <vector>

namespace {

int failures = 0;

void Check(bool condition, const char* what) {
    if (condition) {
        return;
    }
    std::printf("FAIL %s\n", what);
    failures++;
}

void Append(std::vector<uint8_t>& config, std::initializer_list<uint8_t> bytes) {
    config.insert(config.end(), bytes);
}

void AppendInterface(std::vector<uint8_t>& config, uint8_t number, uint8_t alt,
                     uint8_t endpoints, uint8_t subclass) {
    Append(config, {9, 0x04, number, alt, endpoints, 0x01, subclass, 0x20, 0});
}

void AppendEndpoint(std::vector<uint8_t>& config, uint8_t address, uint8_t attributes,
                    uint16_t max_packet) {
    Append(config, {7, 0x05, address, attributes, static_cast<uint8_t>(max_packet & 0xFF),
                    static_cast<uint8_t>(max_packet >> 8), 1});
}

/// The Cayin RU7's real configuration descriptor: three PCM alternate settings and one
/// RAW_DATA setting for native DSD. Transcribed from the same source as the Rust fixture in
/// src/output/usb/descriptors.rs, so the two parsers are held to one device.
std::vector<uint8_t> Ru7() {
    std::vector<uint8_t> config = {9, 0x02, 0, 0, 2, 1, 0, 0x80, 50};

    AppendInterface(config, 0, 0, 0, 0x01);
    Append(config, {9, 0x24, 0x01, 0x00, 0x02, 0x04, 0x40, 0x00, 0x00});
    Append(config, {8, 0x24, 0x0A, 0x05, 0x03, 0x07, 0x00, 0x00});
    Append(config, {17, 0x24, 0x02, 0x01, 0x01, 0x01, 0x00, 0x05, 0x02, 0x03, 0x00, 0x00, 0x00,
                    0x00, 0x00, 0x00, 0x00});

    AppendInterface(config, 1, 0, 0, 0x02);
    const uint8_t pcm[3][3] = {{1, 2, 16}, {2, 3, 24}, {3, 4, 32}};
    for (const auto& setting : pcm) {
        AppendInterface(config, 1, setting[0], 2, 0x02);
        Append(config, {16, 0x24, 0x01, 0x01, 0x05, 0x01, 0x01, 0x00, 0x00, 0x00, 0x02, 0x03,
                        0x00, 0x00, 0x00, 0x00});
        Append(config, {6, 0x24, 0x02, 0x01, setting[1], setting[2]});
        AppendEndpoint(config, 0x01, 0x05, 776);
        AppendEndpoint(config, 0x81, 0x11, 4);
    }
    // Alt 4: bmFormats = 0x80000000, RAW_DATA.
    AppendInterface(config, 1, 4, 2, 0x02);
    Append(config, {16, 0x24, 0x01, 0x01, 0x05, 0x01, 0x00, 0x00, 0x00, 0x80, 0x02, 0x03, 0x00,
                    0x00, 0x00, 0x00});
    Append(config, {6, 0x24, 0x02, 0x01, 0x04, 0x20});
    AppendEndpoint(config, 0x01, 0x05, 776);
    AppendEndpoint(config, 0x81, 0x11, 4);
    return config;
}

const dsd::AltSetting* FindAlt(const dsd::Uac2Layout& layout, uint8_t alt_setting) {
    for (size_t index = 0; index < layout.alt_count; index++) {
        if (layout.alts[index].alt_setting == alt_setting) {
            return &layout.alts[index];
        }
    }
    return nullptr;
}

}  // namespace

int main() {
    const std::vector<uint8_t> ru7 = Ru7();
    dsd::Uac2Layout layout{};
    Check(dsd::ParseLayout(ru7.data(), ru7.size(), &layout), "RU7 descriptor parses");

    Check(layout.control_interface == 0, "control interface is 0");
    Check(layout.streaming_interface == 1, "streaming interface is 1");
    Check(layout.clock_id == 5, "clock source is ID 5");
    Check(layout.alt_count == 4, "alt 0 is dropped, four settings carry audio");

    const dsd::AltSetting* native = FindAlt(layout, 4);
    Check(native != nullptr, "alt 4 is kept");
    if (native != nullptr) {
        Check(native->raw_data, "alt 4 is RAW_DATA");
        Check(native->subslot_bytes == 4, "native DSD uses four-byte subslots");
        Check(native->channels == 2, "native DSD carries two channels");
        Check(native->out_endpoint == 0x01, "native DSD writes to endpoint 1");
        Check(native->feedback_endpoint == 0x81, "native DSD has a feedback endpoint");
        Check(native->max_packet == 776, "native DSD packet is 776 bytes");
    }

    const dsd::AltSetting* pcm24 = FindAlt(layout, 2);
    Check(pcm24 != nullptr && !pcm24->raw_data, "alt 2 is PCM, not RAW_DATA");
    Check(pcm24 != nullptr && pcm24->bit_resolution == 24, "alt 2 carries 24 bits");

    // 776 bytes per microframe over two four-byte subslots is 97 samples, so the endpoint
    // reaches 776000 Hz and stops short of 780000.
    if (native != nullptr) {
        Check(dsd::AltCarriesRate(*native, 705600), "DSD256 native rate fits");
        Check(dsd::AltCarriesRate(*native, 776000), "the endpoint ceiling fits exactly");
        Check(!dsd::AltCarriesRate(*native, 780000), "past the ceiling does not fit");
        Check(!dsd::AltCarriesRate(*native, 0), "a zero rate never fits");
    }

    // Two subranges: one discrete rate, and a walked range of three.
    const uint8_t report[] = {0x02, 0x00,                          // two subranges
                              0x44, 0xAC, 0x00, 0x00,              // min 44100
                              0x44, 0xAC, 0x00, 0x00,              // max 44100
                              0x00, 0x00, 0x00, 0x00,              // resolution 0
                              0x80, 0xBB, 0x00, 0x00,              // min 48000
                              0x60, 0x74, 0x02, 0x00,              // max 160800
                              0x50, 0xDC, 0x00, 0x00};             // resolution 56400
    uint32_t rates[dsd::kMaxSampleRates] = {};
    const size_t count = dsd::ParseClockRange(report, sizeof(report), rates, dsd::kMaxSampleRates);
    Check(count == 4, "one discrete rate and three walked rates");
    Check(count > 0 && rates[0] == 44100, "the discrete subrange is 44100");
    Check(count > 3 && rates[1] == 48000 && rates[2] == 104400 && rates[3] == 160800,
          "the walked subrange steps by its resolution");

    const size_t truncated = dsd::ParseClockRange(report, 10, rates, dsd::kMaxSampleRates);
    Check(truncated == 0, "a subrange cut short is dropped rather than read past");

    dsd::FormatEntry formats[64] = {};
    const uint32_t publish[] = {44100, 352800, 705600, 1411200};
    const size_t written =
        dsd::BuildFormats(layout, publish, 4, formats, sizeof(formats) / sizeof(formats[0]));
    Check(written > 0, "the RU7 publishes formats");
    Check(formats[0].native_dsd, "native DSD leads the format list");
    Check(formats[0].sample_rate == 44100.0, "the native list starts at the lowest rate");

    bool trailing_pcm = false;
    bool native_after_pcm = false;
    bool native_at_1411200 = false;
    for (size_t index = 0; index < written; index++) {
        if (!formats[index].native_dsd) {
            trailing_pcm = true;
            continue;
        }
        if (trailing_pcm) {
            native_after_pcm = true;
        }
        if (formats[index].sample_rate == 1411200.0) {
            native_at_1411200 = true;
        }
    }
    Check(!native_after_pcm, "no native format follows a PCM one");
    // 1411200 Hz needs 1416 bytes a microframe in four-byte subslots, past the 776 the
    // endpoint carries, so DSD512 is not on this DAC's native list.
    Check(!native_at_1411200, "DSD512 exceeds the native endpoint and is not published");

    if (failures == 0) {
        std::printf("ok, all checks passed\n");
        return 0;
    }
    std::printf("%d check(s) failed\n", failures);
    return 1;
}
