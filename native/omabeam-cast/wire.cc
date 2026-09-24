// Copyright 2026 OmaBeam contributors. MIT license.
#include "omabeam/wire.h"
#include <algorithm>
#include <cerrno>
#include <fcntl.h>
#include <unistd.h>
#include "util/json/json_helpers.h"

namespace omabeam {
bool Nonblocking(int fd) {
  const int flags = fcntl(fd, F_GETFL);
  return flags >= 0 && fcntl(fd, F_SETFL, flags | O_NONBLOCK) == 0;
}
bool Reader::Poll(const Callback& callback) {
  // Fairness: bounded work even when a producer continuously writes.
  size_t read_budget = 256 * 1024;
  size_t packets = 0;
  while (packets < 16) {
    if (buffer_.size() >= 4) {
      const uint32_t size = (uint32_t{buffer_[0]} << 24) |
          (uint32_t{buffer_[1]} << 16) | (uint32_t{buffer_[2]} << 8) | buffer_[3];
      if (!size || size > kMaxHeader) {
        error_ = "Invalid IPC header length";
        return false;
      }
      if (buffer_.size() >= 4 + size) {
        auto parsed = openscreen::json::Parse(std::string_view(
            reinterpret_cast<const char*>(buffer_.data() + 4), size));
        if (parsed.is_error() || !parsed.value().isObject()) {
          error_ = "Invalid IPC JSON object";
          return false;
        }
        const auto& header = parsed.value();
        if (header.isMember("bytes") && !header["bytes"].isUInt()) {
          error_ = "Invalid IPC payload length";
          return false;
        }
        const size_t payload = header.get("bytes", 0).asUInt();
        if (payload > (media_ ? kMaxFrame : 0)) {
          error_ = "IPC payload exceeds channel limit";
          return false;
        }
        if (buffer_.size() >= 4 + size + payload) {
          std::vector<uint8_t> data(buffer_.begin() + 4 + size,
                                    buffer_.begin() + 4 + size + payload);
          callback(header, data);
          buffer_.erase(buffer_.begin(), buffer_.begin() + 4 + size + payload);
          ++packets;
          continue;
        }
      }
    }
    if (read_budget == 0) return true;
    uint8_t chunk[8192];
    const ssize_t n = read(fd_, chunk, std::min(sizeof(chunk), read_budget));
    if (n > 0) {
      buffer_.insert(buffer_.end(), chunk, chunk + n);
      read_budget -= static_cast<size_t>(n);
      continue;
    }
    if (n < 0 && errno == EINTR) continue;
    if (n < 0 && (errno == EAGAIN || errno == EWOULDBLOCK)) return true;
    error_ = n == 0 ? (buffer_.empty() ? "IPC closed" : "Truncated IPC packet")
                    : "IPC read failed";
    return false;
  }
  return true;
}
bool Writer::Push(Json::Value event) {
  event["version"] = 1;
  auto serialized = openscreen::json::Stringify(event);
  if (serialized.is_error()) return false;
  std::string json = std::move(serialized.value());
  const size_t size = json.size();
  if (size > kMaxHeader || pending_ + size + 4 > 256 * 1024) return false;
  std::string packet;
  for (int shift : {24, 16, 8, 0}) packet.push_back(static_cast<char>(size >> shift));
  packet += json;
  pending_ += packet.size();
  queue_.push_back(std::move(packet));
  return Flush();
}
bool Writer::Flush() {
  while (!queue_.empty()) {
    const auto& next = queue_.front();
    const ssize_t n = write(fd_, next.data() + offset_, next.size() - offset_);
    if (n > 0) {
      offset_ += n;
      pending_ -= n;
      if (offset_ == next.size()) { queue_.pop_front(); offset_ = 0; }
      continue;
    }
    if (n < 0 && errno == EINTR) continue;
    return n < 0 && (errno == EAGAIN || errno == EWOULDBLOCK);
  }
  return true;
}
}  // namespace omabeam
