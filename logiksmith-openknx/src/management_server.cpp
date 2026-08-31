#include "logiksmith_openknx/management_server.h"

#include <string.h>

namespace logiksmith {
namespace openknx {

namespace {

bool copy_c_string(char* destination, size_t capacity, const char* source) {
    if (destination == nullptr || source == nullptr) {
        return false;
    }
    const size_t length = strlen(source);
    if (length + 1 > capacity) {
        return false;
    }
    memcpy(destination, source, length + 1);
    return true;
}

} // namespace

bool ManagementMailbox::submit(const ManagementRequest& request) {
    if (request.method == nullptr || request.path == nullptr ||
        request.body_length > kHttpMaxBodyBytes ||
        (request.body_length > 0 && request.body == nullptr) || _size >= kHttpMaxDynamicRequests) {
        return false;
    }
    ManagementCommand& command = _commands[_write % kHttpMaxDynamicRequests];
    if (!copy_c_string(command.method, sizeof(command.method), request.method) ||
        !copy_c_string(command.path, sizeof(command.path), request.path)) {
        return false;
    }
    command.body_length = request.body_length;
    if (request.body_length > 0) {
        memcpy(command.body, request.body, request.body_length);
    }
    _write = (_write + 1) % kHttpMaxDynamicRequests;
    ++_size;
    return true;
}

bool ManagementMailbox::pop(ManagementCommand& command) {
    if (_size == 0) {
        return false;
    }
    command = _commands[_read % kHttpMaxDynamicRequests];
    _read = (_read + 1) % kHttpMaxDynamicRequests;
    --_size;
    return true;
}

bool ManagementServer::is_dynamic(const char* path) {
    return path != nullptr && strncmp(path, "/api/", 5) == 0;
}

bool ManagementServer::is_mutation(const ManagementRequest& request) {
    if (request.method == nullptr) {
        return true;
    }
    return strcmp(request.method, "GET") != 0 && strcmp(request.method, "HEAD") != 0;
}

void ManagementServer::error(ManagementResponse& response,
                             HttpStatus status,
                             const char* message) {
    response.status = status;
    response.retry_after_seconds = status == HttpStatus::ServiceUnavailable ? 1 : 0;
    response.body_length = message == nullptr ? 0 : strlen(message);
    if (response.body_length > sizeof(response.body)) {
        response.body_length = sizeof(response.body);
    }
    if (response.body_length > 0) {
        memcpy(response.body, message, response.body_length);
    }
}

void ManagementServer::accept(const ManagementRequest& request,
                              ManagementResponse& response) {
    response = ManagementResponse{};
    if (request.path == nullptr || request.method == nullptr) {
        ++_rejected_requests;
        error(response, HttpStatus::BadRequest, "invalid request");
        return;
    }
    if (request.body_length > kHttpMaxBodyBytes) {
        ++_rejected_requests;
        error(response, HttpStatus::PayloadTooLarge, "request body too large");
        return;
    }
    if (!is_dynamic(request.path)) {
        // Static asset lookup is supplied by the ESP-IDF adapter. Returning a
        // bounded miss here keeps this policy class independent of flash data.
        error(response, HttpStatus::BadRequest, "unknown route");
        return;
    }
    if (is_mutation(request) && !_programming_mode) {
        ++_rejected_requests;
        error(response, HttpStatus::Locked, "programming mode required");
        return;
    }
    if (_dynamic_in_flight >= kHttpMaxDynamicRequests || !_mailbox.submit(request)) {
        ++_rejected_requests;
        error(response, HttpStatus::ServiceUnavailable, "management queue full");
        return;
    }
    ++_dynamic_in_flight;
    response.status = HttpStatus::Ok;
}

void ManagementServer::complete_dynamic_request() {
    if (_dynamic_in_flight > 0) {
        --_dynamic_in_flight;
    }
}

bool ManagementServer::process_one() {
    ManagementCommand command;
    if (!_mailbox.pop(command)) {
        return false;
    }
    if (_handler != nullptr) {
        _handler(command, _handler_context);
    }
    complete_dynamic_request();
    return true;
}

} // namespace openknx
} // namespace logiksmith
