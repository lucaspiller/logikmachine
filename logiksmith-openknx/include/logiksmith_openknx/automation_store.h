#pragma once

#include <stddef.h>
#include <stdint.h>

namespace logiksmith {
namespace openknx {

constexpr size_t kAutomationMaxBytes = 16 * 1024;
constexpr const char* kAutomationPath = "/logiksmith/automation.toml";
constexpr const char* kAutomationTempPath = "/logiksmith/automation.toml.tmp";

// A tiny filesystem seam keeps LittleFS policy testable without linking the
// ESP-IDF filesystem into native tests.
class AutomationFileSystem {
  public:
    virtual ~AutomationFileSystem() = default;
    virtual bool mount() = 0;
    virtual size_t free_bytes() const = 0;
    virtual bool exists(const char* path) const = 0;
    virtual bool read(const char* path,
                      uint8_t* destination,
                      size_t capacity,
                      size_t& written) = 0;
    virtual bool write(const char* path,
                       const uint8_t* bytes,
                       size_t length,
                       size_t& written) = 0;
    virtual bool flush(const char* path) = 0;
    virtual bool rename(const char* from, const char* to) = 0;
    virtual bool remove(const char* path) = 0;
};

enum class AutomationStoreStatus : uint8_t {
    Ready,
    Missing,
    InvalidDocument,
    MountFailed,
    ReadFailed,
    TooLarge,
    Full,
    ShortWrite,
    FlushFailed,
    RenameFailed,
    RemoveFailed,
    ActivationFailed,
    RollbackFailed,
};

using AutomationActivation = bool (*)(const uint8_t* bytes, size_t length, void* context);

class AutomationStore final {
  public:
    explicit AutomationStore(AutomationFileSystem& filesystem) : _filesystem(filesystem) {}

    AutomationStoreStatus load(uint8_t* destination, size_t capacity, size_t& length);
    AutomationStoreStatus load_validated(uint8_t* destination,
                                          size_t capacity,
                                          size_t& length,
                                          bool (*validate)(const uint8_t*, size_t));
    AutomationStoreStatus save(const uint8_t* bytes, size_t length);
    AutomationStoreStatus save_and_activate(const uint8_t* bytes,
                                            size_t length,
                                            AutomationActivation activate,
                                            void* context);

    bool mounted() const { return _mounted; }
    bool runtime_enabled() const { return _runtime_enabled; }
    AutomationStoreStatus last_status() const { return _last_status; }

  private:
    AutomationStoreStatus mount();
    AutomationStoreStatus atomic_write(const uint8_t* bytes, size_t length);

    AutomationFileSystem& _filesystem;
    bool _mounted = false;
    bool _runtime_enabled = false;
    AutomationStoreStatus _last_status = AutomationStoreStatus::MountFailed;
    uint8_t _previous[kAutomationMaxBytes] = {};
    size_t _previous_length = 0;
    bool _previous_exists = false;
};

} // namespace openknx
} // namespace logiksmith
