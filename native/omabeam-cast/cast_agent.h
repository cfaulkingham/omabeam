// Copyright 2020 The Chromium Authors
// Copyright 2026 OmaBeam contributors
// Adapted from Open Screen's LoopingFileCastAgent. See UPSTREAM.md and LICENSE.openscreen.
#ifndef OMABEAM_CAST_AGENT_H_
#define OMABEAM_CAST_AGENT_H_

#include <functional>
#include <memory>
#include <map>
#include <optional>
#include <string>
#include <vector>

#include "cast/common/channel/cast_message_handler.h"
#include "cast/common/channel/cast_socket_message_port.h"
#include "cast/common/channel/connection_namespace_handler.h"
#include "cast/common/channel/virtual_connection_router.h"
#include "cast/common/public/cast_socket.h"
#include "cast/common/public/trust_store.h"
#include "cast/sender/public/sender_socket_factory.h"
#include "cast/streaming/public/environment.h"
#include "cast/streaming/public/sender_session.h"
#include "json/value.h"
#include "platform/impl/task_runner.h"
#include "util/alarm.h"
#include "util/scoped_wake_lock.h"

namespace openscreen::cast {

struct CastSettings {
  IPEndpoint receiver_endpoint;
  int width = 1280;
  int height = 720;
  int fps = 30;
  int max_bitrate = 4000000;
  std::chrono::milliseconds playout_delay{180};
  std::string resume_session;
};

class CastAgent final : public SenderSocketFactory::Client,
                        public VirtualConnectionRouter::SocketErrorHandler,
                        public ConnectionNamespaceHandler::VirtualConnectionPolicy,
                        public CastMessageHandler,
                        public SenderSession::Client,
                        public Sender::Observer {
 public:
  using EventCallback = std::function<void(Json::Value)>;
  CastAgent(TaskRunner& runner, std::unique_ptr<TrustStore> trust,
            EventCallback event, std::function<void()> stopped);
  ~CastAgent();
  void Connect(CastSettings settings);
  void RequestStop();
  void Submit(const Json::Value& header, const std::vector<uint8_t>& bytes);
  void Tick();
  void Fail(std::string code, std::string message);

 private:
  void OnConnected(SenderSocketFactory*, const IPEndpoint&,
                   std::unique_ptr<CastSocket>) override;
  void OnError(SenderSocketFactory*, const IPEndpoint&, const Error&) override;
  void OnClose(CastSocket*) override;
  void OnError(CastSocket*, const Error&) override;
  bool IsConnectionAllowed(const VirtualConnection&) const override;
  void OnMessage(VirtualConnectionRouter*, CastSocket*, proto::CastMessage) override;
  const char* GetStreamingAppId() const;
  void HandleReceiverStatus(const Json::Value&);
  void OnRemoteMessagingOpened(bool);
  void OnReceiverMessagingOpened(bool);
  void CreateAndStartSession();
  void OnNegotiated(const SenderSession*, SenderSession::ConfiguredSenders,
                    capture_recommendations::Recommendations) override;
  void OnError(const SenderSession*, const Error&) override;
  void OnFrameCanceled(FrameId) override;
  void OnPictureLost() override;
  void Shutdown();
  void State(const char* state);

  TaskRunner& task_runner_;
  EventCallback event_;
  std::function<void()> shutdown_callback_;
  VirtualConnectionRouter router_;
  ConnectionNamespaceHandler connection_handler_;
  SenderSocketFactory socket_factory_;
  std::unique_ptr<TlsConnectionFactory> connection_factory_;
  CastSocketMessagePort message_port_;
  int next_request_id_ = 1;
  int launch_request_id_ = -1;
  std::optional<int> receiver_pixel_limit_;
  std::optional<CastSettings> connection_settings_;
  ScopedWakeLockPtr wake_lock_;
  std::string app_session_id_;
  std::optional<VirtualConnection> remote_connection_;
  std::optional<VirtualConnection> platform_remote_connection_;
  std::unique_ptr<Environment> environment_;
  std::unique_ptr<SenderSession> current_session_;
  std::unique_ptr<SenderSession::ConfiguredSenders> current_negotiation_;
  bool has_launched_ = false;
  bool stop_requested_ = false;
  bool shutdown_ = false;
  bool need_keyframe_ = true;
  Alarm stop_ack_timeout_;
  Clock::time_point deadline_{};
  std::optional<Clock::time_point> media_origin_;
  uint64_t last_pts_ = 0;
  uint64_t accepted_ = 0;
  uint64_t released_ = 0;
  uint64_t retransmitted_ = 0;
  uint64_t dropped_ = 0;
  uint64_t sequence_ = 0;
  std::map<FrameId, size_t> in_flight_;
  size_t in_flight_bytes_ = 0;
  Clock::time_point last_feedback_{};
  Clock::time_point next_report_{};
  bool heartbeat_started_ = false;
  Clock::time_point next_heartbeat_{};
  Clock::time_point last_heartbeat_{};
  uint64_t heartbeat_pongs_ = 0;
  int min_bitrate_ = 300000;
  int max_bitrate_ = 4000000;
  int target_bitrate_ = 0;
  void OnPacketsRetransmitted(int count) override;
};
}  // namespace openscreen::cast
#endif
