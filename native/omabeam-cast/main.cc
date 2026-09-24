// Copyright 2026 OmaBeam contributors. MIT license.
#include <csignal>
#include <cstdio>
#include <cstdlib>
#include <memory>
#include <string>
#include <unistd.h>
#include "omabeam/cast_agent.h"
#include "omabeam/discovery.h"
#include "omabeam/wire.h"
#include "platform/impl/platform_client_posix.h"

using namespace openscreen;
using namespace openscreen::cast;

int main(int argc, char** argv) {
  std::signal(SIGPIPE, SIG_IGN);
  int media_fd = -1;
  std::string certificate;
  for (int i = 1; i < argc; ++i) {
    const std::string arg = argv[i];
    if (arg == "--version") {
      std::puts("omabeam-cast protocol=1 openscreen=8b108491d2696309ca37ac0d3260bf423e773e8b");
      return 0;
    }
    if (arg == "--media-fd" && i + 1 < argc) {
      char* end = nullptr;
      const long fd = std::strtol(argv[++i], &end, 10);
      if (!end || *end || fd < 3 || fd > 1024) return 2;
      media_fd = static_cast<int>(fd);
    } else if (arg == "--developer-certificate" && i + 1 < argc) {
      // Explicit test invocation only. Production uses Google's Cast roots.
      certificate = argv[++i];
    } else {
      std::fputs("usage: omabeam-cast [--media-fd N] [--developer-certificate PEM]\n", stderr);
      return 2;
    }
  }
  auto trust = certificate.empty() ? CastTrustStore::Create()
      : TrustStore::CreateInstanceFromPemFile(certificate);
  if (!trust) { std::fputs("Cannot load Cast trust store\n", stderr); return 2; }
  if (!omabeam::Nonblocking(STDIN_FILENO) || !omabeam::Nonblocking(STDOUT_FILENO) ||
      (media_fd >= 0 && !omabeam::Nonblocking(media_fd))) return 2;

  auto* runner = new TaskRunnerImpl(&Clock::now);
  PlatformClientPosix::Create(std::chrono::milliseconds(5),
                             std::unique_ptr<TaskRunnerImpl>(runner));
  omabeam::Reader control(STDIN_FILENO, false);
  omabeam::Reader media(media_fd, true);
  omabeam::Writer events(STDOUT_FILENO);
  std::unique_ptr<CastAgent> agent;
  std::unique_ptr<Discovery> discovery;
  std::unique_ptr<Alarm> poll_alarm;
  bool done = false;
  bool stopping = false;
  bool connected = false;
  bool output_failed = false;
  int result = 0;
  const auto emit = [&](Json::Value event) {
    if (event["event"] == "error") result = 1;
    if (!events.Push(std::move(event))) output_failed = true;
  };
  const auto stopped = [&] {
    done = true;
    runner->RequestStopSoon();
  };
  const auto stop = [&] {
    if (stopping) return;
    stopping = true;
    if (agent) agent->RequestStop();
    else stopped();
  };
  const auto fail = [&](const char* message) {
    Json::Value error;
    error["event"] = "error";
    error["code"] = "protocol";
    error["message"] = message;
    emit(std::move(error));
    stop();
  };
  const auto command = [&](const Json::Value& json, const std::vector<uint8_t>&) {
    if (stopping) return;
    if (!json["version"].isInt() || json["version"].asInt() != 1 ||
        !json["command"].isString()) {
      fail("Unsupported Cast IPC version or command");
      return;
    }
    const std::string action = json["command"].asString();
    if (action == "stop") { stop(); return; }
    if (action == "discover" && !connected && !discovery) {
      discovery = std::make_unique<Discovery>(*runner, emit);
      return;
    }
    if (action != "connect" || connected || media_fd < 0 ||
        !json["endpoint"].isString()) {
      fail("Unexpected Cast command");
      return;
    }
    auto endpoint = IPEndpoint::Parse(json["endpoint"].asString());
    if (endpoint.is_error() || !endpoint.value().address || !endpoint.value().port) {
      fail("Invalid receiver endpoint");
      return;
    }
    for (const char* key : {"width", "height", "fps", "bitrate"}) {
      if (!json[key].isInt()) { fail("Invalid video configuration"); return; }
    }
    CastSettings settings;
    settings.receiver_endpoint = endpoint.value();
    settings.width = json["width"].asInt();
    settings.height = json["height"].asInt();
    settings.fps = json["fps"].asInt();
    settings.max_bitrate = json["bitrate"].asInt();
    if (json.isMember("resume_session")) {
      if (!json["resume_session"].isString() || json["resume_session"].asString().empty() ||
          json["resume_session"].asString().size() > 256) {
        fail("Invalid receiver session identity");
        return;
      }
      settings.resume_session = json["resume_session"].asString();
    }
    if (settings.width < 320 || settings.width > 1920 || settings.width % 2 ||
        settings.height < 240 || settings.height > 1080 || settings.height % 2 ||
        settings.fps < 1 || settings.fps > 30 || settings.max_bitrate < 300000 ||
        settings.max_bitrate > 20000000) {
      fail("Video configuration is outside the supported range");
      return;
    }
    discovery.reset();
    connected = true;
    agent = std::make_unique<CastAgent>(*runner, std::move(trust), emit, stopped);
    agent->Connect(std::move(settings));
  };
  std::function<void()> poll;
  poll = [&] {
    if (done) return;
    if (!stopping && !control.Poll(command)) {
      if (control.error() != "IPC closed") fail(control.error().c_str());
      else stop();
    }
    if (!stopping && media_fd >= 0 && !media.Poll(
        [&](const Json::Value& header, const std::vector<uint8_t>& data) {
          if (stopping) return;
          if (!agent) { fail("Media arrived before connect"); return; }
          agent->Submit(header, data);
        })) {
      if (media.error() != "IPC closed") fail(media.error().c_str());
      else stop();
    }
    if (agent) agent->Tick();
    if (output_failed || !events.Flush()) stop();
    if (!done) poll_alarm->ScheduleFromNow(poll, std::chrono::milliseconds(2));
  };
  runner->PostTask([&] {
    poll_alarm = std::make_unique<Alarm>(&Clock::now, *runner);
    Json::Value hello;
    hello["event"] = "ready";
    hello["codec"] = "h264";
    emit(std::move(hello));
    poll();
  });
  runner->RunUntilSignaled();
  if (!done) {
    runner->PostTask(stop);
    runner->RunUntilStopped();
  }
  runner->PostTask([&] {
    poll_alarm.reset();
    discovery.reset();
    agent.reset();
    events.Flush();
    runner->RequestStopSoon();
  });
  runner->RunUntilStopped();
  PlatformClientPosix::ShutDown();
  return result;
}
