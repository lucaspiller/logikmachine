#pragma once

#include <stddef.h>
#include <stdint.h>

#include "logiksmith_embedded_abi.h"
#include "logiksmith_openknx/runtime_processor.h"

namespace logiksmith {
namespace openknx {

// The default block gives the M14 image a deterministic executable path. In
// native tests the ABI symbols may be weak, while the release build promotes
// them to strong references so an absent Rust archive cannot become a silent
// disabled-runtime firmware image.
class AbiRuntimeProcessor final : public RuntimeProcessor {
  public:
    AbiRuntimeProcessor() = default;
    ~AbiRuntimeProcessor() override;

    bool start();
    void shutdown();
    bool started() const { return _runtime != nullptr; }

    void process_input(const InputEvent& event, RawBindingRouter& router) override;
    void tick(uint32_t now_ms, RawBindingRouter& router) override;

  private:
    static constexpr size_t kEffectCapacity = 16;

    void stop();
    static bool copy_endpoint(const uint8_t* bytes, uint16_t length, EndpointId& endpoint);
    static bool copy_block(const uint8_t* bytes, uint16_t length, EndpointId& block);
    static bool encode_value(const LogiksmithValue& value,
                             uint8_t* payload,
                             uint8_t& payload_size,
                             DptId& dpt);

    LogiksmithRuntime* _runtime = nullptr;
    LogiksmithEffect _effects[kEffectCapacity] = {};
};

} // namespace openknx
} // namespace logiksmith
