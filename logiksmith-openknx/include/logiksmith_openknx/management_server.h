#pragma once

#include <stddef.h>
#include <stdint.h>

namespace logiksmith {
namespace openknx {

constexpr size_t kHttpMaxSockets = 4;
constexpr size_t kHttpMaxDynamicRequests = 2;
constexpr size_t kHttpMaxBodyBytes = 16 * 1024;
constexpr size_t kHttpMaxResponseBytes = 16 * 1024;
constexpr size_t kHttpMaxStaticChunkBytes = 1024;

enum class HttpStatus : uint16_t {
    Ok = 200,
    NoContent = 204,
    BadRequest = 400,
    Locked = 423,
    PayloadTooLarge = 413,
    UnprocessableEntity = 422,
    Conflict = 409,
    ServiceUnavailable = 503,
};

struct ManagementRequest {
    ManagementRequest() = default;
    ManagementRequest(const char* request_method,
                      const char* request_path,
                      const uint8_t* request_body,
                      size_t request_body_length)
        : method(request_method),
          path(request_path),
          body(request_body),
          body_length(request_body_length) {}

    const char* method = nullptr;
    const char* path = nullptr;
    const uint8_t* body = nullptr;
    size_t body_length = 0;
};

struct ManagementResponse {
    HttpStatus status = HttpStatus::BadRequest;
    uint8_t body[kHttpMaxResponseBytes] = {};
    size_t body_length = 0;
    uint16_t retry_after_seconds = 0;
    bool gzip = false;
};

struct ManagementCommand {
    char method[8] = {};
    char path[128] = {};
    uint8_t body[kHttpMaxBodyBytes] = {};
    size_t body_length = 0;
};

class ManagementMailbox final {
  public:
    bool submit(const ManagementRequest& request);
    bool pop(ManagementCommand& command);
    size_t size() const { return _size; }

  private:
    ManagementCommand _commands[kHttpMaxDynamicRequests] = {};
    size_t _read = 0;
    size_t _write = 0;
    size_t _size = 0;
};

// Native HTTP policy used by the eventual ESP-IDF adapter. It owns no socket
// or task; those remain platform glue around this bounded admission gate.
class ManagementServer final {
  public:
    using CommandHandler = void (*)(const ManagementCommand&, void* context);

    explicit ManagementServer(ManagementMailbox& mailbox) : _mailbox(mailbox) {}

    void set_programming_mode(bool enabled) { _programming_mode = enabled; }
    bool programming_mode() const { return _programming_mode; }
    size_t dynamic_in_flight() const { return _dynamic_in_flight; }
    uint32_t rejected_requests() const { return _rejected_requests; }

    void accept(const ManagementRequest& request, ManagementResponse& response);
    void complete_dynamic_request();
    void set_command_handler(CommandHandler handler, void* context) {
        _handler = handler;
        _handler_context = context;
    }
    bool process_one();

  private:
    static bool is_dynamic(const char* path);
    static bool is_mutation(const ManagementRequest& request);
    static void error(ManagementResponse& response, HttpStatus status, const char* message);

    ManagementMailbox& _mailbox;
    bool _programming_mode = false;
    size_t _dynamic_in_flight = 0;
    uint32_t _rejected_requests = 0;
    CommandHandler _handler = nullptr;
    void* _handler_context = nullptr;
};

} // namespace openknx
} // namespace logiksmith
