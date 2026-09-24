// Copyright 2026 OmaBeam contributors. MIT license.
#ifndef OMABEAM_WIRE_H_
#define OMABEAM_WIRE_H_
#include <cstdint>
#include <deque>
#include <functional>
#include <string>
#include <vector>
#include "json/value.h"

namespace omabeam {
// Every packet is a big-endian u32 JSON length, JSON, then `bytes` payload.
// Control packets cannot carry media. Reads and writes never block the Cast
// task runner. Limits apply before any untrusted length is allocated.
constexpr size_t kMaxHeader = 4096;
constexpr size_t kMaxFrame = 2 * 1024 * 1024;
bool Nonblocking(int fd);
class Reader {
 public:
  Reader(int fd, bool media) : fd_(fd), media_(media) {}
  using Callback = std::function<void(const Json::Value&, const std::vector<uint8_t>&)>;
  bool Poll(const Callback& callback);
  const std::string& error() const { return error_; }
 private:
  int fd_;
  bool media_;
  std::vector<uint8_t> buffer_;
  std::string error_;
};
class Writer {
 public:
  explicit Writer(int fd) : fd_(fd) {}
  bool Push(Json::Value event);
  bool Flush();
 private:
  int fd_;
  std::deque<std::string> queue_;
  size_t offset_ = 0;
  size_t pending_ = 0;
};
}  // namespace omabeam
#endif
