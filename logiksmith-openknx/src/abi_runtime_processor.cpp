#include "logiksmith_openknx/abi_runtime_processor.h"

#include <string.h>

// Native host tests can exercise the boundary without a Rust archive, but a
// release image must pull the archive and fail at link time if it is absent.
#if defined(LOGIKSMITH_REQUIRE_ABI_RUNTIME)
#define LOGIKSMITH_ABI_WEAK
#elif defined(__APPLE__)
#define LOGIKSMITH_ABI_WEAK __attribute__((weak_import))
#else
#define LOGIKSMITH_ABI_WEAK __attribute__((weak))
#endif
extern "C" uint32_t logiksmith_abi_version(void) LOGIKSMITH_ABI_WEAK;
extern "C" int32_t logiksmith_runtime_create(
    const LogiksmithRuntimeConfig* config,
    LogiksmithRuntime** out_runtime) LOGIKSMITH_ABI_WEAK;
extern "C" int32_t logiksmith_runtime_destroy(LogiksmithRuntime* runtime)
    LOGIKSMITH_ABI_WEAK;
extern "C" int32_t logiksmith_runtime_process_input(
    LogiksmithRuntime* runtime,
    const LogiksmithInputEvent* event,
    LogiksmithEffect* effects,
    size_t capacity,
    size_t* written) LOGIKSMITH_ABI_WEAK;
extern "C" int32_t logiksmith_runtime_process_due_timer(
    LogiksmithRuntime* runtime,
    uint64_t monotonic_ms,
    LogiksmithEffect* effects,
    size_t capacity,
    size_t* written) LOGIKSMITH_ABI_WEAK;
#undef LOGIKSMITH_ABI_WEAK

#if !defined(LOGIKSMITH_REQUIRE_ABI_RUNTIME)
// Keep the M14 native test/link seam usable when the expanded M15 timer
// symbol is not supplied by a Rust archive yet. A strong Rust definition wins
// in release images.
extern "C" __attribute__((weak)) int32_t logiksmith_runtime_process_due_timer(
    LogiksmithRuntime*,
    uint64_t,
    LogiksmithEffect*,
    size_t,
    size_t*) {
    return LOGIKSMITH_STATUS_INVALID_CONFIGURATION;
}
#endif

namespace logiksmith {
namespace openknx {

AbiRuntimeProcessor::~AbiRuntimeProcessor() {
    stop();
}

bool AbiRuntimeProcessor::start() {
    if (_runtime != nullptr || logiksmith_abi_version == nullptr ||
        logiksmith_runtime_create == nullptr ||
        logiksmith_runtime_process_input == nullptr ||
        logiksmith_runtime_destroy == nullptr ||
        logiksmith_abi_version() != LOGIKSMITH_ABI_VERSION) {
        return false;
    }

    static const uint8_t trigger_name[] = "trigger";
    static const uint8_t light_name[] = "light";
    static const uint8_t block_name[] = "main";
    static const uint8_t source[] =
        "function handle(event)\n"
        "  if event.type == 'input' and event.input == 'trigger' and event.value == true then\n"
        "    return { outputs = { light = true } }\n"
        "  end\n"
        "  return {}\n"
        "end\n";
    const LogiksmithEndpointConfig endpoints[] = {
        {trigger_name, sizeof(trigger_name) - 1, LOGIKSMITH_ENDPOINT_INPUT, {0, 0, 0}, 1,
         1},
        {light_name, sizeof(light_name) - 1, LOGIKSMITH_ENDPOINT_OUTPUT, {0, 0, 0}, 1,
         1},
    };
    const LogiksmithBlockConfig block = {
        block_name,
        sizeof(block_name) - 1,
        source,
        sizeof(source) - 1,
        endpoints,
        sizeof(endpoints) / sizeof(endpoints[0]),
    };
    const LogiksmithRuntimeConfig config = {&block, 1};

    LogiksmithRuntime* runtime = nullptr;
    if (logiksmith_runtime_create(&config, &runtime) != LOGIKSMITH_STATUS_OK) {
        return false;
    }
    _runtime = runtime;
    return true;
}

void AbiRuntimeProcessor::shutdown() {
    stop();
}

void AbiRuntimeProcessor::stop() {
    if (_runtime != nullptr && logiksmith_runtime_destroy != nullptr) {
        (void)logiksmith_runtime_destroy(_runtime);
    }
    _runtime = nullptr;
}

bool AbiRuntimeProcessor::copy_endpoint(const uint8_t* bytes,
                                        uint16_t length,
                                        EndpointId& endpoint) {
    if (bytes == nullptr || length == 0 || length > kMaxEndpointName) {
        return false;
    }
    char value[kMaxEndpointName + 1] = {};
    memcpy(value, bytes, length);
    return endpoint.assign(value);
}

bool AbiRuntimeProcessor::copy_block(const uint8_t* bytes,
                                      uint16_t length,
                                      EndpointId& block) {
    return copy_endpoint(bytes, length, block);
}

bool AbiRuntimeProcessor::encode_value(const LogiksmithValue& value,
                                       uint8_t* payload,
                                       uint8_t& payload_size,
                                       DptId& dpt) {
    if (payload == nullptr || value.reserved != 0 || value.reserved2 != 0) {
        return false;
    }
    dpt = DptId(value.dpt_major, value.dpt_subtype);
    if (dpt == kDptBool && value.kind == LOGIKSMITH_VALUE_BOOL &&
        (value.scalar == 0 || value.scalar == 1)) {
        payload[0] = value.scalar == 0 ? 0 : 1;
        payload_size = 1;
        return true;
    }
    if (dpt == kDptPercent && value.kind == LOGIKSMITH_VALUE_PERCENT &&
        value.scalar >= 0 && value.scalar <= 100) {
        payload[0] = static_cast<uint8_t>((value.scalar * 255 + 50) / 100);
        payload_size = 1;
        return true;
    }
    if (dpt == kDptTemperature &&
        value.kind == LOGIKSMITH_VALUE_TEMPERATURE_CENTI_DEGREES) {
        int32_t mantissa = value.scalar;
        uint8_t exponent = 0;
        while ((mantissa > 2047 || mantissa < -2048) && exponent < 15) {
            mantissa = mantissa >= 0 ? (mantissa + 1) / 2 : (mantissa - 1) / 2;
            ++exponent;
        }
        if (mantissa > 2047 || mantissa < -2048) {
            return false;
        }
        const uint16_t encoded_mantissa = static_cast<uint16_t>(mantissa) & 0x07FFU;
        const uint16_t raw = static_cast<uint16_t>((mantissa < 0 ? 0x8000U : 0U) |
                                                    (static_cast<uint16_t>(exponent) << 11U) |
                                                    encoded_mantissa);
        payload[0] = static_cast<uint8_t>(raw >> 8U);
        payload[1] = static_cast<uint8_t>(raw & 0xFFU);
        payload_size = 2;
        return true;
    }
    return false;
}

void AbiRuntimeProcessor::process_input(const InputEvent& event,
                                        RawBindingRouter& router) {
    if (_runtime == nullptr ||
        (event.dpt == kDptBool && event.payload_size != 1) ||
        (event.dpt == kDptPercent && event.payload_size != 1) ||
        (event.dpt == kDptTemperature && event.payload_size != 2) ||
        (event.dpt != kDptBool && event.dpt != kDptPercent &&
         event.dpt != kDptTemperature)) {
        return;
    }

    const EndpointId fallback_block = [] {
        EndpointId value;
        value.assign("main");
        return value;
    }();
    const EndpointId& block = event.block_id.empty() ? fallback_block : event.block_id;
    const uint8_t* block_bytes = reinterpret_cast<const uint8_t*>(block.c_str());
    LogiksmithInputEvent input = {};
    input.block_id = block_bytes;
    input.block_id_len = block.size();
    input.endpoint = reinterpret_cast<const uint8_t*>(event.endpoint.c_str());
    input.endpoint_len = event.endpoint.size();
    input.value = {event.dpt.main,
                   event.dpt.subtype,
                   static_cast<uint8_t>(event.dpt == kDptBool
                       ? LOGIKSMITH_VALUE_BOOL
                       : (event.dpt == kDptPercent
                              ? LOGIKSMITH_VALUE_PERCENT
                              : LOGIKSMITH_VALUE_TEMPERATURE_CENTI_DEGREES)),
                   0,
                   0,
                   event.dpt == kDptBool
                       ? (event.bool_value() ? 1 : 0)
                       : (event.dpt == kDptPercent ? event.percent_value()
                                                   : event.temperature_centi_degrees())};
    input.source_address = event.source_address;
    input.group_address = event.group_address;
    input.monotonic_ms = event.received_at_ms;

    size_t written = 0;
    const int32_t status = logiksmith_runtime_process_input(
        _runtime, &input, _effects, kEffectCapacity, &written);
    if (status != LOGIKSMITH_STATUS_OK || written > kEffectCapacity) {
        return;
    }

    for (size_t index = 0; index < written; ++index) {
        const LogiksmithEffect& effect = _effects[index];
        EndpointId effect_block;
        if (!copy_block(effect.block_id, effect.block_id_len, effect_block)) {
            continue;
        }
        EndpointId endpoint;
        if (!copy_endpoint(effect.endpoint, effect.endpoint_len, endpoint)) {
            continue;
        }
        uint8_t payload[kMaxTelegramPayload] = {};
        uint8_t payload_size = 0;
        DptId dpt;
        if (!encode_value(effect.value, payload, payload_size, dpt)) {
            continue;
        }
        (void)router.enqueue_output(effect_block, endpoint, dpt, payload, payload_size);
    }
}

void AbiRuntimeProcessor::tick(uint32_t now_ms, RawBindingRouter& router) {
    if (_runtime == nullptr || logiksmith_runtime_process_due_timer == nullptr) {
        return;
    }
    size_t written = 0;
    const int32_t status = logiksmith_runtime_process_due_timer(
        _runtime, now_ms, _effects, kEffectCapacity, &written);
    if (status != LOGIKSMITH_STATUS_OK || written > kEffectCapacity) {
        return;
    }
    for (size_t index = 0; index < written; ++index) {
        const LogiksmithEffect& effect = _effects[index];
        EndpointId block;
        EndpointId endpoint;
        if (!copy_block(effect.block_id, effect.block_id_len, block) ||
            !copy_endpoint(effect.endpoint, effect.endpoint_len, endpoint)) {
            continue;
        }
        uint8_t payload[kMaxTelegramPayload] = {};
        uint8_t payload_size = 0;
        DptId dpt;
        if (encode_value(effect.value, payload, payload_size, dpt)) {
            (void)router.enqueue_output(block, endpoint, dpt, payload, payload_size);
        }
    }
}

} // namespace openknx
} // namespace logiksmith
