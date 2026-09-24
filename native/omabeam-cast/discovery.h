// Copyright 2026 OmaBeam contributors. MIT license.
#ifndef OMABEAM_DISCOVERY_H_
#define OMABEAM_DISCOVERY_H_
#include <functional>
#include <set>
#include <string>
#include "cast/common/public/receiver_info.h"
#include "discovery/common/reporting_client.h"
#include "discovery/public/dns_sd_service_factory.h"
#include "discovery/public/dns_sd_service_watcher.h"
#include "json/value.h"

namespace openscreen::cast {
class Discovery final : public discovery::ReportingClient {
 public:
  Discovery(TaskRunner& runner, std::function<void(Json::Value)> event);
  ~Discovery() override;
 private:
  void OnFatalError(const Error& error) override;
  void OnRecoverableError(const Error& error) override;
  std::function<void(Json::Value)> event_;
  discovery::DnsSdServicePtr service_;
  std::unique_ptr<discovery::DnsSdServiceWatcher<ReceiverInfo>> watcher_;
  std::set<std::string> known_;
};
}  // namespace openscreen::cast
#endif
