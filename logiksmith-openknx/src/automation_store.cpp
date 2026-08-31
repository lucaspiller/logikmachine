#include "logiksmith_openknx/automation_store.h"

namespace logiksmith {
namespace openknx {

AutomationStoreStatus AutomationStore::mount() {
    if (_mounted) {
        return AutomationStoreStatus::Ready;
    }
    if (!_filesystem.mount()) {
        _runtime_enabled = false;
        _last_status = AutomationStoreStatus::MountFailed;
        return _last_status;
    }
    _mounted = true;
    return AutomationStoreStatus::Ready;
}

AutomationStoreStatus AutomationStore::load(uint8_t* destination,
                                            size_t capacity,
                                            size_t& length) {
    length = 0;
    const AutomationStoreStatus mounted = mount();
    if (mounted != AutomationStoreStatus::Ready) {
        return mounted;
    }
    if (!_filesystem.exists(kAutomationPath)) {
        _runtime_enabled = false;
        _last_status = AutomationStoreStatus::Missing;
        return _last_status;
    }
    size_t read = 0;
    if (!_filesystem.read(kAutomationPath, destination, capacity, read)) {
        _runtime_enabled = false;
        _last_status = AutomationStoreStatus::ReadFailed;
        return _last_status;
    }
    if (read > capacity || read > kAutomationMaxBytes) {
        _runtime_enabled = false;
        _last_status = AutomationStoreStatus::TooLarge;
        return _last_status;
    }
    length = read;
    _runtime_enabled = true;
    _last_status = AutomationStoreStatus::Ready;
    return _last_status;
}

AutomationStoreStatus AutomationStore::load_validated(uint8_t* destination,
                                                      size_t capacity,
                                                      size_t& length,
                                                      bool (*validate)(const uint8_t*, size_t)) {
    const AutomationStoreStatus status = load(destination, capacity, length);
    if (status != AutomationStoreStatus::Ready || validate == nullptr) {
        if (status == AutomationStoreStatus::Ready && validate == nullptr) {
            _runtime_enabled = false;
            _last_status = AutomationStoreStatus::InvalidDocument;
            return _last_status;
        }
        return status;
    }
    if (!validate(destination, length)) {
        // Preserve the file on disk. Recovery is an explicit programming-mode
        // action; malformed user data must not be silently replaced.
        _runtime_enabled = false;
        _last_status = AutomationStoreStatus::InvalidDocument;
        return _last_status;
    }
    return AutomationStoreStatus::Ready;
}

AutomationStoreStatus AutomationStore::atomic_write(const uint8_t* bytes, size_t length) {
    if (bytes == nullptr || length == 0 || length > kAutomationMaxBytes) {
        return AutomationStoreStatus::TooLarge;
    }
    if (_filesystem.free_bytes() < length) {
        return AutomationStoreStatus::Full;
    }
    size_t written = 0;
    if (!_filesystem.write(kAutomationTempPath, bytes, length, written)) {
        return written == length ? AutomationStoreStatus::ReadFailed
                                  : AutomationStoreStatus::ShortWrite;
    }
    if (written != length) {
        return AutomationStoreStatus::ShortWrite;
    }
    if (!_filesystem.flush(kAutomationTempPath)) {
        (void)_filesystem.remove(kAutomationTempPath);
        return AutomationStoreStatus::FlushFailed;
    }
    if (!_filesystem.rename(kAutomationTempPath, kAutomationPath)) {
        (void)_filesystem.remove(kAutomationTempPath);
        return AutomationStoreStatus::RenameFailed;
    }
    return AutomationStoreStatus::Ready;
}

AutomationStoreStatus AutomationStore::save(const uint8_t* bytes, size_t length) {
    const AutomationStoreStatus mounted = mount();
    if (mounted != AutomationStoreStatus::Ready) {
        return mounted;
    }
    const AutomationStoreStatus status = atomic_write(bytes, length);
    _runtime_enabled = status == AutomationStoreStatus::Ready;
    _last_status = status;
    return status;
}

AutomationStoreStatus AutomationStore::save_and_activate(const uint8_t* bytes,
                                                         size_t length,
                                                         AutomationActivation activate,
                                                         void* context) {
    const AutomationStoreStatus mounted = mount();
    if (mounted != AutomationStoreStatus::Ready) {
        return mounted;
    }
    if (bytes == nullptr || length == 0 || length > kAutomationMaxBytes || activate == nullptr) {
        _last_status = AutomationStoreStatus::TooLarge;
        return _last_status;
    }

    _previous_exists = _filesystem.exists(kAutomationPath);
    _previous_length = 0;
    if (_previous_exists &&
        !_filesystem.read(kAutomationPath, _previous, sizeof(_previous), _previous_length)) {
        _last_status = AutomationStoreStatus::ReadFailed;
        return _last_status;
    }

    const AutomationStoreStatus saved = atomic_write(bytes, length);
    if (saved != AutomationStoreStatus::Ready) {
        _last_status = saved;
        return saved;
    }
    if (activate(bytes, length, context)) {
        _runtime_enabled = true;
        _last_status = AutomationStoreStatus::Ready;
        return _last_status;
    }

    const AutomationStoreStatus rollback = _previous_exists
                                               ? atomic_write(_previous, _previous_length)
                                               : (_filesystem.remove(kAutomationPath)
                                                      ? AutomationStoreStatus::Ready
                                                      : AutomationStoreStatus::RemoveFailed);
    _runtime_enabled = rollback == AutomationStoreStatus::Ready && _previous_exists;
    _last_status = rollback == AutomationStoreStatus::Ready
                       ? AutomationStoreStatus::ActivationFailed
                       : AutomationStoreStatus::RollbackFailed;
    return _last_status;
}

} // namespace openknx
} // namespace logiksmith
