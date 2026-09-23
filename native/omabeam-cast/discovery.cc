// Copyright 2026 OmaBeam contributors. MIT license.
#include "omabeam/discovery.h"
#include <algorithm>
#include <map>
#include <sstream>
#include "discovery/common/config.h"
#include "platform/impl/network_interface.h"
#include "util/osp_logging.h"

namespace openscreen::cast {
Discovery::Discovery(TaskRunner& runner, std::function<void(Json::Value)> callback)
    : event_(std::move(callback)) {
  std::vector<InterfaceInfo> interfaces;
  for (const auto& iface : GetNetworkInterfaces()) {
    if ((iface.type == InterfaceInfo::Type::kEthernet ||
         iface.type == InterfaceInfo::Type::kWifi) && !iface.addresses.empty()) {
      interfaces.push_back(iface);
    }
  }
  discovery::Config config{.network_info = std::move(interfaces),
                           .enable_publication = false, .enable_querying = true};
  service_ = discovery::CreateDnsSdService(runner, *this, std::move(config));
  watcher_ = std::make_unique<discovery::DnsSdServiceWatcher<ReceiverInfo>>(
      service_.get(), kCastV2ServiceId, DnsSdInstanceEndpointToReceiverInfo,
      [this](std::vector<std::reference_wrapper<const ReceiverInfo>> all) {
        std::set<std::string> current;
        std::map<std::string, Json::Value> receivers;
        for (const ReceiverInfo& info : all) {
          if (!info.IsValid() || !(info.capabilities & kHasVideoOutput) ||
              (!info.v4_address && !info.v6_address) ||
              (receivers.size() >= 128 && !receivers.contains(info.unique_id)) ||
              info.unique_id.size() > 256 || info.friendly_name.size() > 256 ||
              info.model_name.size() > 256) continue;
          auto& event = receivers[info.unique_id];
          event["event"] = "receiver";
          event["id"] = info.unique_id;
          event["name"] = info.friendly_name;
          event["model"] = info.model_name;
          event["busy"] = event["busy"].asBool() || info.status == kBusy;
          if (!event["addresses"].isArray()) event["addresses"] = Json::arrayValue;
          for (const auto& address : {info.v4_address, info.v6_address}) {
            if (!address) continue;
            std::ostringstream endpoint;
            if (address.IsLinkLocal()) {
              // Rust SocketAddr accepts numeric scope IDs, not interface
              // names. Never publish an unusable unscoped link-local address.
              if (!address.GetScopeId()) continue;
              endpoint << '[' << IPAddress(address.version(), address.bytes())
                       << '%' << address.GetScopeId() << "]:" << info.port;
            } else {
              endpoint << IPEndpoint{address, info.port};
            }
            auto& addresses = event["addresses"];
            const Json::Value value(endpoint.str());
            if (addresses.size() < 16 && std::find(addresses.begin(), addresses.end(), value) == addresses.end())
              addresses.append(value);
          }
        }
        for (auto& [id, event] : receivers) {
          if (event["addresses"].empty()) continue;
          current.insert(id);
          event_(std::move(event));
        }
        for (const auto& id : known_) {
          if (current.contains(id)) continue;
          Json::Value event;
          event["event"] = "receiver_removed";
          event["id"] = id;
          event_(std::move(event));
        }
        known_ = std::move(current);
      });
  watcher_->StartDiscovery();
}
Discovery::~Discovery() = default;
void Discovery::OnFatalError(const Error& error) {
  Json::Value event;
  event["event"] = "error";
  event["code"] = "discovery";
  event["message"] = error.message();
  event_(std::move(event));
}
void Discovery::OnRecoverableError(const Error& error) {
  OSP_LOG_WARN << "Discovery: " << error;
}
}  // namespace openscreen::cast
