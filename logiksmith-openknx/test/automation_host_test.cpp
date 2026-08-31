#include "logiksmith_openknx/automation_store.h"
#include "logiksmith_openknx/management_server.h"

#include <assert.h>
#include <string.h>

using namespace logiksmith::openknx;

namespace {

class FakeFileSystem final : public AutomationFileSystem {
  public:
    bool mount() override { return mount_ok; }
    size_t free_bytes() const override { return free_size; }
    bool exists(const char* path) const override {
        if (strcmp(path, kAutomationPath) == 0) {
            return main_exists;
        }
        if (strcmp(path, kAutomationTempPath) == 0) {
            return temp_exists;
        }
        return false;
    }
    bool read(const char* path, uint8_t* destination, size_t capacity, size_t& written) override {
        if (!read_ok || destination == nullptr) {
            written = 0;
            return false;
        }
        const uint8_t* source = nullptr;
        size_t length = 0;
        if (strcmp(path, kAutomationPath) == 0 && main_exists) {
            source = main;
            length = main_length;
        } else if (strcmp(path, kAutomationTempPath) == 0 && temp_exists) {
            source = temp;
            length = temp_length;
        } else {
            written = 0;
            return false;
        }
        written = length;
        if (capacity >= length) {
            memcpy(destination, source, length);
        }
        return true;
    }
    bool write(const char* path,
               const uint8_t* bytes,
               size_t length,
               size_t& written) override {
        if (!write_ok || bytes == nullptr) {
            written = 0;
            return false;
        }
        uint8_t* destination = nullptr;
        size_t* destination_length = nullptr;
        if (strcmp(path, kAutomationTempPath) == 0) {
            destination = temp;
            destination_length = &temp_length;
            temp_exists = true;
        } else {
            written = 0;
            return false;
        }
        written = length < max_write ? length : max_write;
        memcpy(destination, bytes, written);
        *destination_length = written;
        return written == length;
    }
    bool flush(const char*) override { return flush_ok; }
    bool rename(const char*, const char*) override {
        if (!rename_ok) {
            return false;
        }
        memcpy(main, temp, temp_length);
        main_length = temp_length;
        main_exists = true;
        temp_exists = false;
        return true;
    }
    bool remove(const char* path) override {
        if (strcmp(path, kAutomationPath) == 0) {
            main_exists = false;
            main_length = 0;
        } else {
            temp_exists = false;
            temp_length = 0;
        }
        return true;
    }

    bool mount_ok = true;
    bool read_ok = true;
    bool write_ok = true;
    bool flush_ok = true;
    bool rename_ok = true;
    size_t free_size = kAutomationMaxBytes;
    size_t max_write = kAutomationMaxBytes;
    bool main_exists = false;
    bool temp_exists = false;
    uint8_t main[kAutomationMaxBytes] = {};
    uint8_t temp[kAutomationMaxBytes] = {};
    size_t main_length = 0;
    size_t temp_length = 0;
};

bool valid_document(const uint8_t* bytes, size_t length) {
    return bytes != nullptr && length >= 3 && bytes[0] == 'o' && bytes[1] == 'k';
}

bool fail_activation(const uint8_t*, size_t, void*) { return false; }

unsigned handled = 0;
void count_command(const ManagementCommand&, void*) { ++handled; }

void storage_failures_are_bounded_and_rollback() {
    FakeFileSystem fs;
    AutomationStore store(fs);
    uint8_t bytes[16] = {};
    size_t length = 0;
    assert(store.load(bytes, sizeof(bytes), length) == AutomationStoreStatus::Missing);
    const uint8_t old[] = "ok-old";
    assert(store.save(old, sizeof(old) - 1) == AutomationStoreStatus::Ready);
    assert(store.load_validated(bytes, sizeof(bytes), length, valid_document) ==
           AutomationStoreStatus::Ready);
    assert(length == sizeof(old) - 1);

    const uint8_t invalid[] = "bad";
    assert(store.save(invalid, sizeof(invalid) - 1) == AutomationStoreStatus::Ready);
    assert(store.load_validated(bytes, sizeof(bytes), length, valid_document) ==
           AutomationStoreStatus::InvalidDocument);
    assert(fs.main[0] == 'b');
    assert(store.save(old, sizeof(old) - 1) == AutomationStoreStatus::Ready);

    fs.max_write = 2;
    assert(store.save(old, sizeof(old) - 1) == AutomationStoreStatus::ShortWrite);
    fs.max_write = kAutomationMaxBytes;
    fs.free_size = 1;
    assert(store.save(old, sizeof(old) - 1) == AutomationStoreStatus::Full);
    fs.free_size = kAutomationMaxBytes;
    fs.flush_ok = false;
    assert(store.save(old, sizeof(old) - 1) == AutomationStoreStatus::FlushFailed);
    fs.flush_ok = true;
    fs.rename_ok = false;
    assert(store.save(old, sizeof(old) - 1) == AutomationStoreStatus::RenameFailed);
    fs.rename_ok = true;

    const uint8_t newer[] = "ok-new";
    assert(store.save_and_activate(newer, sizeof(newer) - 1, fail_activation, nullptr) ==
           AutomationStoreStatus::ActivationFailed);
    assert(fs.main_exists && fs.main_length == sizeof(old) - 1 &&
           memcmp(fs.main, old, sizeof(old) - 1) == 0);
}

void management_gate_and_mailbox_are_bounded() {
    ManagementMailbox mailbox;
    ManagementServer server(mailbox);
    server.set_command_handler(count_command, nullptr);
    ManagementResponse response;
    const ManagementRequest mutation{"POST", "/api/blocks/main/source", nullptr, 0};
    server.accept(mutation, response);
    assert(response.status == HttpStatus::Locked);

    server.set_programming_mode(true);
    const ManagementRequest get{"GET", "/api/meta", nullptr, 0};
    server.accept(get, response);
    assert(response.status == HttpStatus::Ok);
    server.accept(get, response);
    assert(response.status == HttpStatus::Ok);
    server.accept(get, response);
    assert(response.status == HttpStatus::ServiceUnavailable);
    assert(server.process_one());
    assert(server.dynamic_in_flight() == 1);
    assert(server.process_one());
    assert(server.dynamic_in_flight() == 0);
    assert(handled == 2);

    uint8_t oversized[kHttpMaxBodyBytes + 1] = {};
    const ManagementRequest too_large{"POST", "/api/blocks/main/source", oversized,
                                     sizeof(oversized)};
    server.accept(too_large, response);
    assert(response.status == HttpStatus::PayloadTooLarge);
}

} // namespace

int main() {
    storage_failures_are_bounded_and_rollback();
    management_gate_and_mailbox_are_bounded();
    return 0;
}
