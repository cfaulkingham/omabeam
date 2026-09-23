// Copyright 2020 The Chromium Authors
// Copyright 2026 OmaBeam contributors
// Adapted from Open Screen. See UPSTREAM.md and LICENSE.openscreen.

#include "omabeam/cast_agent.h"

#include <format>
#include <algorithm>
#include <optional>
#include <string>
#include <utility>
#include <vector>

#include "build/build_config.h"
#include "cast/common/channel/message_util.h"
#include "cast/common/public/cast_streaming_app_ids.h"
#include "cast/streaming/public/capture_recommendations.h"
#include "cast/streaming/public/constants.h"
#include "cast/streaming/public/offer_messages.h"
#include "cast/streaming/public/receiver_message.h"
#include "json/value.h"
#include "platform/api/tls_connection_factory.h"
#include "util/json/json_helpers.h"
#include "util/trace_logging.h"

namespace openscreen::cast {
namespace {

using DeviceMediaPolicy = SenderSocketFactory::DeviceMediaPolicy;

}  // namespace

CastAgent::CastAgent(
    TaskRunner& task_runner,
    std::unique_ptr<TrustStore> cast_trust_store,
    EventCallback event, std::function<void()> shutdown_callback)
    : task_runner_(task_runner),
      event_(std::move(event)),
      shutdown_callback_(std::move(shutdown_callback)),
      connection_handler_(router_, *this),
      socket_factory_(*this,
                      task_runner_,
                      std::move(cast_trust_store),
                      CastCRLTrustStore::Create()),
      connection_factory_(
          TlsConnectionFactory::CreateFactory(socket_factory_, task_runner_)),
      message_port_(router_),
      stop_ack_timeout_(&Clock::now, task_runner_) {
  router_.AddHandlerForLocalId(kPlatformSenderId, this);
  socket_factory_.set_factory(connection_factory_.get());
}

CastAgent::~CastAgent() {
  // Best-effort: if a session is active, make one immediate attempt to tell
  // the receiver to stop it, but don't wait around for confirmation.
  RequestStop();
}

void CastAgent::Connect(CastSettings settings) {
  TRACE_DEFAULT_SCOPED(TraceCategory::kStandaloneSender);

  OSP_CHECK(!connection_settings_);
  connection_settings_ = std::move(settings);
  deadline_ = Clock::now() + std::chrono::seconds(45);
  target_bitrate_ = std::min(connection_settings_->max_bitrate, 4000000);
  State("authenticating");

  task_runner_.PostTask([this] {
    if (shutdown_ || stop_requested_) return;
#if BUILDFLAG(IS_APPLE) || BUILDFLAG(IS_LINUX)
    wake_lock_ = ScopedWakeLock::Create(task_runner_);
#endif  // BUILDFLAG(IS_APPLE) || BUILDFLAG(IS_LINUX)
    socket_factory_.Connect(connection_settings_->receiver_endpoint, DeviceMediaPolicy::kIncludesVideo,
                            &router_);
  });
}

void CastAgent::OnConnected(SenderSocketFactory* factory,
                                       const IPEndpoint& endpoint,
                                       std::unique_ptr<CastSocket> socket) {
  TRACE_DEFAULT_SCOPED(TraceCategory::kStandaloneSender);

  if (shutdown_ || stop_requested_) return;

  if (message_port_.GetSocketId() != ToCastSocketId(nullptr)) {
    OSP_LOG_WARN << "Already connected, dropping peer at: " << endpoint;
    return;
  }
  message_port_.SetSocket(socket->GetWeakPtr());
  router_.TakeSocket(this, std::move(socket));

  State(connection_settings_->resume_session.empty() ? "launching" : "reconnecting");
  // First, CONNECT to the platform receiver.
  platform_remote_connection_.emplace(VirtualConnection{
      kPlatformSenderId, kPlatformReceiverId, message_port_.GetSocketId()});
  connection_handler_.OpenRemoteConnection(
      *platform_remote_connection_,
      [this](bool success) { OnReceiverMessagingOpened(success); });
}

void CastAgent::OnError(SenderSocketFactory* factory,
                                   const IPEndpoint& endpoint,
                                   const Error& error) {
  OSP_LOG_ERROR << "Cast agent received socket factory error: " << error;
  const auto code = error.code();
  const bool network = code == Error::Code::kConnectionFailed ||
      code == Error::Code::kSocketConnectFailure ||
      code == Error::Code::kSocketClosedFailure ||
      code == Error::Code::kSocketReadFailure || code == Error::Code::kSocketSendFailure;
  Fail(network ? "connection" : "authentication",
       error.message().empty() ? ToString(code) : error.message());
}

void CastAgent::OnClose(CastSocket* cast_socket) {
  OSP_VLOG << "Cast agent socket closed.";
  if (stop_requested_ || shutdown_) Shutdown();
  else Fail("connection", "Receiver control connection closed");
}

void CastAgent::OnError(CastSocket* socket, const Error& error) {
  OSP_LOG_ERROR << "Cast agent received socket error: " << error;
  Fail("connection", error.message().empty() ? ToString(error.code()) : error.message());
}

bool CastAgent::IsConnectionAllowed(
    const VirtualConnection& virtual_conn) const {
  return !shutdown_ && platform_remote_connection_ &&
      virtual_conn.socket_id == platform_remote_connection_->socket_id;
}

void CastAgent::OnMessage(VirtualConnectionRouter* router,
                                     CastSocket* socket,
                                     proto::CastMessage message) {
  if (shutdown_ || stop_requested_) return;
  if (message.namespace_() == kHeartbeatNamespace &&
      message_port_.GetSocketId() == ToCastSocketId(socket)) {
    const auto payload = json::Parse(GetPayload(message));
    if (payload.is_error() || !payload.value().isObject()) return;
    if (HasType(payload.value(), CastMessageType::kPing)) {
      last_heartbeat_ = Clock::now();
      router_.Send(VirtualConnection{message.destination_id(), message.source_id(),
                                     message_port_.GetSocketId()},
                   MakeSimpleUTF8Message(kHeartbeatNamespace, R"({"type":"PONG"})"));
    } else if (HasType(payload.value(), CastMessageType::kPong)) {
      last_heartbeat_ = Clock::now();
      ++heartbeat_pongs_;
    }
    return;
  }
  if (message_port_.GetSocketId() == ToCastSocketId(socket) &&
      !message_port_.source_id().empty() &&
      message_port_.source_id() == message.destination_id()) {
    OSP_CHECK_NE(message.destination_id(), kPlatformSenderId);
    // Preserve the distinction between an explicit receiver constraint and
    // Open Screen's advisory default pixel-rate estimate. The latter is lower
    // than its default 1080p30 dimensions and is not a decoder capability.
    const auto json_message = json::Parse(GetPayload(message));
    if (json_message.is_value()) {
      auto parsed = ReceiverMessage::Parse(json_message.value());
      if (parsed.is_value() && parsed.value().valid &&
          parsed.value().type == ReceiverMessage::Type::kAnswer) {
        const auto& answer = std::get<Answer>(parsed.value().body);
        receiver_pixel_limit_ = answer.constraints
            ? answer.constraints->video.max_pixels_per_second : std::nullopt;
      }
    }
    message_port_.OnMessage(router, socket, std::move(message));
    return;
  }

  if (message.destination_id() != kPlatformSenderId &&
      message.destination_id() != kBroadcastId) {
    return;  // Message not for us.
  }

  if (message.namespace_() == kReceiverNamespace &&
      message_port_.GetSocketId() == ToCastSocketId(socket)) {
    if (message.payload_type() != proto::CastMessage::STRING) {
      OSP_DLOG_WARN << ": received an unsupported BINARY type message.";
    }

    const ErrorOr<Json::Value> payload = json::Parse(GetPayload(message));
    if (payload.is_error()) {
      OSP_LOG_ERROR << "Failed to parse message: " << payload.error();
      return;
    }

    if (!payload.value().isObject()) {
      OSP_LOG_ERROR << "Parsed message is not a JSON object";
      return;
    }

    if (HasType(payload.value(), CastMessageType::kReceiverStatus)) {
      HandleReceiverStatus(payload.value());
    } else if (HasType(payload.value(), CastMessageType::kLaunchError)) {
      std::string reason;
      if (!json::TryParseString(payload.value()[kMessageKeyReason], &reason)) {
        reason = "UNKNOWN";
      }
      OSP_LOG_ERROR
          << "Failed to launch the Cast Mirroring App on the Receiver! Reason: "
          << reason;
      Fail("launch", reason);
    } else if (HasType(payload.value(), CastMessageType::kInvalidRequest)) {
      std::string reason;
      if (!json::TryParseString(payload.value()[kMessageKeyReason], &reason)) {
        reason = "UNKNOWN";
      }
      OSP_LOG_ERROR << "Cast Receiver thinks our request is invalid: "
                    << reason;
    }
  }
}

const char* CastAgent::GetStreamingAppId() const {
  return GetCastStreamingAudioVideoAppId();
}

void CastAgent::HandleReceiverStatus(const Json::Value& status) {
  if (!has_launched_ && (!status[kMessageKeyRequestId].isInt() ||
      status[kMessageKeyRequestId].asInt() != launch_request_id_)) return;
  const Json::Value& details =
      (status[kMessageKeyStatus].isObject() &&
       status[kMessageKeyStatus][kMessageKeyApplications].isArray())
          ? status[kMessageKeyStatus][kMessageKeyApplications][0]
          : Json::Value();

  std::string running_app_id;
  if (!has_launched_ && !connection_settings_->resume_session.empty() &&
      (details[kMessageKeyAppId] != GetStreamingAppId() ||
       details[kMessageKeySessionId] != connection_settings_->resume_session)) {
    Fail("receiver_replaced", "The receiver no longer has this mirroring session; start Cast again");
    return;
  }
  if (!json::TryParseString(details[kMessageKeyAppId], &running_app_id) ||
      running_app_id != GetStreamingAppId()) {
    if (has_launched_) {
      // The mirroring app is not running and should have already been launched.
      // The receiver has already told us it's gone (whether because we asked it
      // to stop, or for some other reason), so there is nothing left to request
      // -- just tear down locally. If it has been stopped already, Shutdown()
      // is a no-op.
      Shutdown();
    }
    return;
  }

  // If the mirroring app is the current streaming application, we can now
  // safely say we have been launched.
  has_launched_ = true;

  std::string session_id;
  if (!json::TryParseString(details[kMessageKeySessionId], &session_id) ||
      session_id.empty()) {
    OSP_LOG_ERROR
        << "Cannot continue: Cast Receiver did not provide a session ID for "
           "the Mirroring App running on it.";
    RequestStop();
    return;
  }
  if (app_session_id_ != session_id) {
    if (app_session_id_.empty()) {
      app_session_id_ = session_id;
    } else {
      OSP_LOG_ERROR << "Cannot continue: Different Mirroring App session is "
                       "now running on the Cast Receiver.";
      Shutdown();
      return;
    }
  }

  if (remote_connection_) {
    // The mirroring app is running and this CastAgent is already
    // streaming to it (or is awaiting message routing to be established). There
    // are no additional actions to be taken in response to this extra
    // RECEIVER_STATUS message.
    return;
  }

  std::string message_destination_id;
  if (!json::TryParseString(details[kMessageKeyTransportId],
                            &message_destination_id) ||
      message_destination_id.empty()) {
    OSP_LOG_ERROR
        << "Cannot continue: Cast Receiver did not provide a transport ID for "
           "routing messages to the Mirroring App running on it.";
    RequestStop();
    return;
  }

  remote_connection_.emplace(
      VirtualConnection{MakeUniqueSessionId("streaming_sender"),
                        message_destination_id, message_port_.GetSocketId()});
  OSP_LOG_INFO << "Starting-up message routing to the Cast Receiver's "
                  "Mirroring App (sessionId="
               << app_session_id_ << ")...";
  connection_handler_.OpenRemoteConnection(
      *remote_connection_,
      [this](bool success) { OnRemoteMessagingOpened(success); });
}

void CastAgent::OnRemoteMessagingOpened(bool success) {
  if (!remote_connection_) {
    return;  // Shutdown() was called in the meantime.
  }

  if (success) {
    OSP_LOG_INFO << "Starting streaming session...";
    CreateAndStartSession();
  } else {
    OSP_LOG_INFO << "Failed to establish messaging to the Cast Receiver's "
                    "Mirroring App. Perhaps another Cast Sender is using it?";
    // The app is (or was) launched under a session we know about; ask the
    // receiver to stop it rather than just abandoning it running.
    RequestStop();
  }
}

void CastAgent::OnReceiverMessagingOpened(bool success) {
  if (shutdown_ || stop_requested_ || !platform_remote_connection_) return;
  // We established a platform connection and now need to launch.
  OSP_CHECK(platform_remote_connection_);
  OSP_CHECK(!remote_connection_);
  if (!success) {
    OSP_LOG_INFO << "Failed to establish messaging to the Cast Receiver.";
    Shutdown();  // Never launched; nothing to notify.
    return;
  }

  heartbeat_started_ = true;
  last_heartbeat_ = Clock::now();
  next_heartbeat_ = last_heartbeat_ + std::chrono::seconds(5);
  if (!connection_settings_->resume_session.empty()) {
    launch_request_id_ = next_request_id_++;
    router_.Send(*platform_remote_connection_, MakeSimpleUTF8Message(
        kReceiverNamespace, std::format(R"({{"type":"GET_STATUS","requestId":{}}})", launch_request_id_)));
    return;
  }

  static constexpr char kLaunchMessageTemplate[] =
      R"({{"type":"LAUNCH", "requestId":{}, "appId":"{}", "language": "en-US",
       "supportedAppTypes":["WEB"]}})";
  launch_request_id_ = next_request_id_++;
  router_.Send(*platform_remote_connection_,
               MakeSimpleUTF8Message(
                   kReceiverNamespace,
                   std::format(kLaunchMessageTemplate, launch_request_id_,
                               GetStreamingAppId())));
}

void CastAgent::CreateAndStartSession() {
  TRACE_DEFAULT_SCOPED(TraceCategory::kStandaloneSender);

  OSP_CHECK(remote_connection_.has_value());
  environment_ =
      std::make_unique<Environment>(&Clock::now, task_runner_, IPEndpoint{});

  SenderSession::Configuration config{
      connection_settings_->receiver_endpoint.address,
      *this,
      environment_.get(),
      &message_port_,
      remote_connection_->local_id,
      remote_connection_->peer_id,
      true,
      false};
  current_session_ = std::make_unique<SenderSession>(std::move(config));
  VideoCaptureConfig config_video;
  config_video.codec = VideoCodec::kH264;
  config_video.max_frame_rate = {connection_settings_->fps, 1};
  config_video.max_bit_rate = connection_settings_->max_bitrate;
  config_video.target_playout_delay = connection_settings_->playout_delay;
  config_video.resolutions.emplace_back(
      Resolution{connection_settings_->width, connection_settings_->height});
  State("negotiating");
  auto error = current_session_->Negotiate({}, {config_video});
  if (!error.ok()) Fail("negotiation", error.message());
}

void CastAgent::OnNegotiated(
    const SenderSession*, SenderSession::ConfiguredSenders senders,
    capture_recommendations::Recommendations recommendations) {
  if (shutdown_ || stop_requested_) return;
  if (!senders.video_sender || senders.video_config.codec != VideoCodec::kH264) {
    Fail("unsupported_codec", "Receiver did not accept H.264 video");
    return;
  }
  const auto& video = recommendations.video;
  const Dimensions requested{connection_settings_->width,
                             connection_settings_->height,
                             {connection_settings_->fps, 1}};
  if (!video.maximum.IsSupersetOf(requested) ||
      (receiver_pixel_limit_ && requested.effective_bit_rate() > *receiver_pixel_limit_)) {
    Fail("unsupported_resolution", "Receiver recommends a lower video size or frame rate");
    return;
  }
  max_bitrate_ = std::min(connection_settings_->max_bitrate, video.bit_rate_limits.maximum);
  min_bitrate_ = std::max(300000, video.bit_rate_limits.minimum);
  if (max_bitrate_ < min_bitrate_) {
    Fail("unsupported_bitrate", "Receiver bitrate constraints cannot be satisfied");
    return;
  }
  target_bitrate_ = std::clamp(target_bitrate_, min_bitrate_, max_bitrate_);
  current_negotiation_ = std::make_unique<SenderSession::ConfiguredSenders>(std::move(senders));
  current_negotiation_->video_sender->SetObserver(this);
  Json::Value event;
  event["event"] = "negotiated";
  event["width"] = connection_settings_->width;
  event["height"] = connection_settings_->height;
  event["fps"] = connection_settings_->fps;
  event["bitrate"] = target_bitrate_;
  event["codec"] = "h264";
  event["receiver_session"] = app_session_id_;
  event_(std::move(event));
  need_keyframe_ = true;
  last_feedback_ = Clock::now();
}

void CastAgent::OnError(const SenderSession*, const Error& error) {
  Fail("negotiation", error.message());
}

void CastAgent::State(const char* state) {
  Json::Value event;
  event["event"] = "state";
  event["state"] = state;
  event_(std::move(event));
}

void CastAgent::Fail(std::string code, std::string message) {
  if (shutdown_ || stop_requested_) return;
  const bool resumable = code == "connection" || code == "receiver_timeout";
  Json::Value event;
  event["event"] = "error";
  event["code"] = std::move(code);
  event["message"] = message.substr(0, 1024);
  event_(std::move(event));
  // Preserve the receiver app after transport loss. The host may query the
  // identical session to resume it; recovery must never issue LAUNCH.
  // Never destroy a SenderSession from inside its negotiation callback.
  if (resumable) {
    stop_requested_ = true;
    task_runner_.PostTask([this] { Shutdown(); });
  } else {
    task_runner_.PostTask([this] { RequestStop(); });
  }
}

void CastAgent::Submit(const Json::Value& header, const std::vector<uint8_t>& bytes) {
  if (shutdown_ || stop_requested_) return;
  if (!current_negotiation_) {
    Fail("protocol", "Video arrived before negotiation");
    return;
  }
  if (!header["pts_us"].isUInt64() || !header["sequence"].isUInt64() ||
      !header["capture_age_us"].isUInt64() || !header["keyframe"].isBool() ||
      bytes.empty() || bytes.size() > 2 * 1024 * 1024) {
    Fail("protocol", "Invalid video frame header");
    return;
  }
  const uint64_t pts = header["pts_us"].asUInt64();
  const uint64_t sequence = header["sequence"].asUInt64();
  const uint64_t age = header["capture_age_us"].asUInt64();
  const bool keyframe = header["keyframe"].asBool();
  if (pts > 7ULL * 24 * 3600 * 1000000 || age > 10000000 ||
      sequence <= sequence_ || (sequence_ && pts < last_pts_ + 1000)) {
    Fail("protocol", "Nonmonotonic or out-of-range video timestamp/sequence");
    return;
  }
  if (sequence != sequence_ + 1) need_keyframe_ = true;
  sequence_ = sequence;
  last_pts_ = pts;
  auto& sender = *current_negotiation_->video_sender;
  const auto now = Clock::now();
  if (!media_origin_) media_origin_ = now - std::chrono::microseconds(age + pts);
  EncodedFrame frame;
  frame.frame_id = sender.GetNextFrameId();
  frame.referenced_frame_id = keyframe ? frame.frame_id : frame.frame_id - 1;
  frame.dependency = keyframe ? EncodedFrame::Dependency::kKeyFrame
                              : EncodedFrame::Dependency::kDependent;
  frame.rtp_timestamp = RtpTimeTicks::FromTimeSinceOrigin(
      std::chrono::microseconds(pts), sender.config().rtp_timebase);
  frame.reference_time = *media_origin_ + std::chrono::microseconds(pts);
  frame.capture_begin_time = frame.reference_time;
  frame.capture_end_time = frame.reference_time;
  frame.data = ByteView(bytes.data(), bytes.size());
  bool accepted = false;
  if (!((need_keyframe_ || sender.NeedsKeyFrame()) && !keyframe) &&
      age < 500000 && now - frame.reference_time < std::chrono::milliseconds(500) &&
      in_flight_bytes_ + bytes.size() <= 8 * 1024 * 1024 &&
      sender.GetInFlightMediaDuration(frame.rtp_timestamp) <= sender.GetMaxInFlightMediaDuration()) {
    // EnqueueFrame encrypts/copies synchronously; `bytes` need not outlive it.
    accepted = sender.EnqueueFrame(frame) == Sender::OK;
  }
  if (accepted) {
    in_flight_[frame.frame_id] = bytes.size();
    in_flight_bytes_ += bytes.size();
    need_keyframe_ = false;
    if (++accepted_ == 1) State("streaming");
  } else {
    ++dropped_;
    // Skipping an encoded H.264 delta breaks the reference chain. Never feed
    // subsequent deltas until the encoder supplies a new SPS/PPS + IDR.
    need_keyframe_ = true;
  }
  Json::Value event;
  event["event"] = "frame";
  event["sequence"] = Json::UInt64(sequence);
  event["accepted"] = accepted;
  event["keyframe"] = need_keyframe_ || sender.NeedsKeyFrame();
  event_(std::move(event));
}

void CastAgent::OnFrameCanceled(FrameId id) {
  const auto found = in_flight_.find(id);
  if (found != in_flight_.end()) {
    in_flight_bytes_ -= found->second;
    in_flight_.erase(found);
    ++released_;
    last_feedback_ = Clock::now();
  }
}
void CastAgent::OnPictureLost() {
  need_keyframe_ = true;
  Json::Value event;
  event["event"] = "keyframe";
  event_(std::move(event));
}
void CastAgent::OnPacketsRetransmitted(int count) {
  retransmitted_ += std::max(count, 0);
}

void CastAgent::Tick() {
  if (shutdown_ || stop_requested_ || !connection_settings_) return;
  const auto now = Clock::now();
  if (heartbeat_started_) {
    if (now - last_heartbeat_ > std::chrono::seconds(15)) {
      Fail("connection", "Receiver control heartbeat timed out");
      return;
    }
    if (now >= next_heartbeat_ && platform_remote_connection_) {
      next_heartbeat_ = now + std::chrono::seconds(5);
      const auto error = router_.Send(*platform_remote_connection_,
          MakeSimpleUTF8Message(kHeartbeatNamespace, R"({"type":"PING"})"));
      if (!error.ok() && error.code() != Error::Code::kAgain) {
        Fail("connection", "Could not send receiver heartbeat");
        return;
      }
    }
  }
  if (!current_negotiation_) {
    if (now > deadline_) Fail("timeout", "Cast connection/negotiation timed out");
    return;
  }
  if (accepted_ && now - last_feedback_ > std::chrono::seconds(10)) {
    Fail("receiver_timeout", "Receiver stopped acknowledging video");
    return;
  }
  if (now < next_report_) return;
  next_report_ = now + std::chrono::seconds(1);
  auto& sender = *current_negotiation_->video_sender;
  const int estimate = current_session_->GetEstimatedNetworkBandwidth();
  const int available = std::clamp(static_cast<int>(estimate * 0.8), min_bitrate_, max_bitrate_);
  if (estimate > 0) target_bitrate_ = std::min(available, target_bitrate_ + 150000);
  Json::Value event;
  event["event"] = "feedback";
  event["bitrate"] = target_bitrate_;
  event["keyframe"] = need_keyframe_ || sender.NeedsKeyFrame();
  event["in_flight_bytes"] = Json::UInt64(in_flight_bytes_);
  event["accepted"] = Json::UInt64(accepted_);
  event["released"] = Json::UInt64(released_);
  event["dropped"] = Json::UInt64(dropped_);
  event["retransmitted_packets"] = Json::UInt64(retransmitted_);
  event["control_heartbeats"] = Json::UInt64(heartbeat_pongs_);
  event["rtt_us"] = Json::Int64(std::chrono::duration_cast<std::chrono::microseconds>(
      sender.GetCurrentRoundTripTime()).count());
  event_(std::move(event));
}


void CastAgent::Shutdown() {
  if (shutdown_) return;
  shutdown_ = true;
  TRACE_DEFAULT_SCOPED(TraceCategory::kStandaloneSender);
  // No-op if nothing is scheduled (e.g. this wasn't reached via
  // RequestStop(), or the receiver's confirmation beat the timeout).
  stop_ack_timeout_.Cancel();

  // Cleared here, rather than in RequestStop() as soon as STOP is sent, so
  // that `app_session_id_` stays accurate for the whole time a STOP may still
  // be outstanding. In particular, HandleReceiverStatus() uses an empty
  // `app_session_id_` to mean "no session -- adopt whatever the receiver
  // reports next"; clearing it early would make a stale/duplicate
  // RECEIVER_STATUS that still echoes the old (soon-to-be-stopped) session
  // during RequestStop()'s wait window look like a brand new one to adopt.
  app_session_id_.clear();

  current_negotiation_.reset();
  if (current_session_) {
    OSP_LOG_INFO << "Stopping mirroring session...";
    current_session_.reset();


  }
  OSP_CHECK(message_port_.source_id().empty());
  environment_.reset();

  if (platform_remote_connection_) {
    const VirtualConnection connection = *platform_remote_connection_;
    // Reset `platform_remote_connection_` because ConnectionNamespaceHandler
    // may call-back into OnReceiverMessagingOpened().
    platform_remote_connection_.reset();
    connection_handler_.CloseRemoteConnection(connection);
  }

  if (remote_connection_) {
    const VirtualConnection connection = *remote_connection_;
    // Reset `remote_connection_` because ConnectionNamespaceHandler may
    // call-back into OnRemoteMessagingOpened().
    remote_connection_.reset();
    connection_handler_.CloseRemoteConnection(connection);
  }

  if (message_port_.GetSocketId() != ToCastSocketId(nullptr)) {
    router_.CloseSocket(message_port_.GetSocketId());
    message_port_.SetSocket({});
  }

  wake_lock_.reset();
  State("ended");

  if (shutdown_callback_) {
    const auto callback = std::move(shutdown_callback_);
    callback();
  }
}

void CastAgent::RequestStop() {
  if (stop_requested_ || shutdown_) {
    // A previous, independent call already sent STOP (or determined there
    // was nothing to stop) and is either waiting for confirmation or has
    // already finished. Don't act again -- in particular, don't fall through
    // to Shutdown() below, which would tear down the connection before that
    // first call's STOP message has had a chance to actually reach the wire.
    return;
  }
  stop_requested_ = true;

  if (!app_session_id_.empty()) {
      OSP_LOG_INFO << "Stopping the Cast Receiver's Mirroring App...";
      Json::Value stop;
      stop["type"] = "STOP";
      stop["requestId"] = next_request_id_++;
      stop["sessionId"] = app_session_id_;
      router_.Send(
          *platform_remote_connection_,
          MakeSimpleUTF8Message(kReceiverNamespace, json::Stringify(stop).value()));

      // Give the Cast Receiver a bounded opportunity to confirm the STOP (a
      // subsequent RECEIVER_STATUS showing the app has stopped calls
      // Shutdown() directly from HandleReceiverStatus(), which cancels this
      // timeout) before forcibly tearing down the connection via Shutdown().
      // Without this, closing the socket immediately after queuing the STOP
      // message races with the underlying async write path and can drop the
      // message before it ever reaches the receiver.
      static constexpr Clock::duration kStopAckTimeout =
          std::chrono::milliseconds(500);
      stop_ack_timeout_.ScheduleFromNow([this] { Shutdown(); },
                                        kStopAckTimeout);
      return;
  }

  Shutdown();
}

}  // namespace openscreen::cast
