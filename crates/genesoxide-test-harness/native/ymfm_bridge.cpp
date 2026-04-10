#include <cstdint>
#include <memory>

#include "ymfm_opn.h"

namespace {

class NullInterface : public ymfm::ymfm_interface {
};

struct Ymfm2612Handle {
    NullInterface intf;
    ymfm::ym2612 chip;

    Ymfm2612Handle() : chip(intf) {
        chip.reset();
    }
};

} // namespace

extern "C" {

Ymfm2612Handle *ymfm2612_create() {
    return new Ymfm2612Handle();
}

void ymfm2612_destroy(Ymfm2612Handle *handle) {
    delete handle;
}

void ymfm2612_reset(Ymfm2612Handle *handle) {
    if (handle != nullptr) {
        handle->chip.reset();
    }
}

std::uint32_t ymfm2612_sample_rate(Ymfm2612Handle *handle, std::uint32_t input_clock) {
    return handle == nullptr ? 0 : handle->chip.sample_rate(input_clock);
}

void ymfm2612_write(Ymfm2612Handle *handle, std::uint8_t port, std::uint8_t reg, std::uint8_t value) {
    if (handle == nullptr) {
        return;
    }

    if ((port & 1U) == 0) {
        handle->chip.write_address(reg);
        handle->chip.write_data(value);
    } else {
        handle->chip.write_address_hi(reg);
        handle->chip.write_data_hi(value);
    }
}

void ymfm2612_generate(Ymfm2612Handle *handle, std::int32_t *left, std::int32_t *right) {
    if (handle == nullptr || left == nullptr || right == nullptr) {
        return;
    }

    ymfm::ym2612::output_data output;
    handle->chip.generate(&output, 1);
    *left = output.data[0];
    *right = output.data[1];
}

} // extern "C"
