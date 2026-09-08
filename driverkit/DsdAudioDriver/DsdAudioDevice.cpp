//
//  DsdAudioDevice.cpp
//

#include <DsdAudioDriver/DsdAudioDevice.h>

#include <AudioDriverKit/AudioDriverKit.h>
#include <DriverKit/DriverKit.h>
#include <DriverKit/IOLib.h>

#include <DsdAudioDriver/DsdAudioDriver.h>

struct DsdAudioDevice_IVars {
    DsdAudioDriver* owner;
};

bool DsdAudioDevice::init(IOUserAudioDriver* in_driver, bool in_supports_prewarming,
                          OSString* in_device_uid, OSString* in_model_uid,
                          OSString* in_manufacturer_uid, uint32_t in_zero_timestamp_period) {
    if (!super::init(in_driver, in_supports_prewarming, in_device_uid, in_model_uid,
                     in_manufacturer_uid, in_zero_timestamp_period)) {
        return false;
    }
    ivars = IONewZero(DsdAudioDevice_IVars, 1);
    return ivars != nullptr;
}

void DsdAudioDevice::free() {
    IOSafeDeleteNULL(ivars, DsdAudioDevice_IVars, 1);
    super::free();
}

void DsdAudioDevice::SetOwner(DsdAudioDriver* in_owner) {
    if (ivars != nullptr) {
        ivars->owner = in_owner;
    }
}

/// Republish the geometry once the change has been made, not before: the rate is read back
/// off the device, and until `super` has run it is still the one being left behind.
///
/// The timestamp period goes out from here and nowhere else. `SetZeroTimeStampPeriod` is
/// only legal during a configuration change, and this is one: IO has stopped and the host
/// re-reads the device when the call returns.
kern_return_t DsdAudioDevice::PerformDeviceConfigurationChange(uint64_t in_change_action,
                                                               OSObject* in_change_info) {
    const kern_return_t result =
        super::PerformDeviceConfigurationChange(in_change_action, in_change_info);
    if (ivars != nullptr && ivars->owner != nullptr) {
        const uint32_t rate = static_cast<uint32_t>(GetSampleRate());
        ivars->owner->PublishTimestampPeriod(rate);
        ivars->owner->PublishGeometry(rate);
    }
    return result;
}

/// The rate arrives here directly, and this is the hook a nominal rate change takes. Both
/// routes end in the same place, and publishing the same values twice costs nothing.
kern_return_t DsdAudioDevice::HandleChangeSampleRate(double in_sample_rate) {
    const kern_return_t result = super::HandleChangeSampleRate(in_sample_rate);
    if (result == kIOReturnSuccess && ivars != nullptr && ivars->owner != nullptr) {
        ivars->owner->PublishGeometry(static_cast<uint32_t>(in_sample_rate));
    }
    return result;
}
