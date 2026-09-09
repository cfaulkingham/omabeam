//! In-process LocalSend client used to hand a live-share URL to another machine.
//!
//! OmaBeam speaks the protocol itself: it enumerates peers on the LAN
//! (multicast announcements, then a subnet probe of port 53317) and uploads the
//! link as a text message. The LocalSend app is not launched or required here.
//! The receiving computer still needs a LocalSend-compatible app (OmaSend or
//! LocalSend) open so it can accept the transfer.

use anyhow::{Context, Result, bail, ensure};
use localsend::{
    crypto::cert::generate_self_signed,
    discovery::{self, DeviceIdentity, DiscoveryConfig, DiscoveryHandle, StatefulDevice},
    http::{
        client::{ClientError, LsHttpClientV2},
        dto_v2::RegisterDtoV2,
    },
    model::{
        discovery::{DeviceType, PROTOCOL_VERSION_V2, ProtocolType},
        transfer::FileDto,
    },
    multicast::{DEFAULT_MULTICAST_GROUP, DEFAULT_MULTICAST_GROUP_V6, DEFAULT_PORT},
};
use std::{
    collections::HashMap,
    net::{IpAddr, Ipv4Addr},
    process::{Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

const FILE_ID: &str = "0";
const ACCEPT_TIMEOUT: Duration = Duration::from_secs(300);
const SCAN_GRACE: Duration = Duration::from_secs(2);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Device {
    pub fingerprint: String,
    pub alias: String,
    pub model: String,
    pub host: String,
    pub port: u16,
    pub protocol: ProtocolType,
}

#[derive(Clone, Debug)]
pub struct SenderInfo {
    pub alias: String,
    pub fingerprint: String,
    pub private_key_pem: String,
    pub certificate_pem: String,
}

impl SenderInfo {
    pub fn generate() -> Result<Self> {
        let cert = generate_self_signed().context("Could not create a LocalSend identity")?;
        Ok(Self {
            alias: machine_name(),
            fingerprint: cert.fingerprint,
            private_key_pem: cert.private_key_pem,
            certificate_pem: cert.certificate_pem,
        })
    }

    fn register(&self) -> RegisterDtoV2 {
        RegisterDtoV2 {
            alias: self.alias.clone(),
            version: PROTOCOL_VERSION_V2.into(),
            device_model: Some("OmaBeam".into()),
            device_type: Some(DeviceType::Desktop),
            fingerprint: self.fingerprint.clone(),
            port: DEFAULT_PORT,
            protocol: ProtocolType::Https,
            download: false,
        }
    }
}

pub struct Discovery {
    handle: Arc<DiscoveryHandle>,
    stop: Option<oneshot::Sender<()>>,
    scanning: Arc<AtomicBool>,
    multicast_error: Option<String>,
}

impl Discovery {
    pub async fn start(info: &SenderInfo) -> Result<Self> {
        let (stop_tx, stop_rx) = oneshot::channel();
        let handle = discovery::start(
            DiscoveryConfig {
                group: DEFAULT_MULTICAST_GROUP,
                group_v6: Some(DEFAULT_MULTICAST_GROUP_V6),
                port: DEFAULT_PORT,
                interface_filter: Default::default(),
                device: localsend::multicast::MulticastDevice {
                    alias: info.alias.clone(),
                    version: PROTOCOL_VERSION_V2.into(),
                    device_model: Some("OmaBeam".into()),
                    device_type: Some(DeviceType::Desktop),
                    fingerprint: info.fingerprint.clone(),
                    port: DEFAULT_PORT,
                    protocol: ProtocolType::Https,
                    download: false,
                },
                identity: DeviceIdentity {
                    cert_pem: info.certificate_pem.clone(),
                    private_key_pem: info.private_key_pem.clone(),
                },
                timeout: discovery::DEFAULT_DISCOVERY_TIMEOUT,
                event_tx: None,
            },
            stop_rx,
        )
        .await;
        let multicast_error = handle.multicast_error().map(|error| error.to_string());
        let discovery = Self {
            handle: Arc::new(handle),
            stop: Some(stop_tx),
            scanning: Arc::new(AtomicBool::new(false)),
            multicast_error,
        };
        discovery.spawn_scan();
        Ok(discovery)
    }

    pub fn devices(&self) -> Vec<Device> {
        let mut devices: Vec<_> = self
            .handle
            .devices()
            .iter()
            .filter_map(device_from)
            .collect();
        devices.sort_by(|a, b| {
            a.alias
                .to_ascii_lowercase()
                .cmp(&b.alias.to_ascii_lowercase())
                .then_with(|| a.fingerprint.cmp(&b.fingerprint))
        });
        devices
    }

    pub fn scanning(&self) -> bool {
        self.scanning.load(Ordering::Relaxed)
    }

    pub fn hint(&self) -> String {
        if !self.devices().is_empty() {
            return "Choose a device. They will be asked to accept the link.".into();
        }
        if self.scanning() {
            "Scanning the local network…".into()
        } else if self.multicast_error.is_some() {
            "Could not listen for announcements. Scanning the local network…".into()
        } else {
            "Looking for devices on this network…".into()
        }
    }

    fn spawn_scan(&self) {
        let handle = self.handle.clone();
        let scanning = self.scanning.clone();
        let immediate = self.multicast_error.is_some();
        tokio::spawn({
            let handle = handle.clone();
            async move {
                let _ = handle
                    .discover("127.0.0.1", DEFAULT_PORT, ProtocolType::Https)
                    .await;
            }
        });
        tokio::spawn(async move {
            if !immediate {
                tokio::time::sleep(SCAN_GRACE).await;
                if !handle.devices().is_empty() {
                    return;
                }
            }
            scanning.store(true, Ordering::Relaxed);
            for ip in local_ipv4s() {
                let _ = handle
                    .scan_subnet(ip, DEFAULT_PORT, ProtocolType::Https)
                    .await;
            }
            scanning.store(false, Ordering::Relaxed);
        });
    }
}

impl Drop for Discovery {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
    }
}

pub fn text_file(text: &str) -> FileDto {
    FileDto {
        id: FILE_ID.into(),
        file_name: "Message.txt".into(),
        size: text.len() as u64,
        file_type: "text/plain".into(),
        sha256: None,
        preview: Some(text.chars().take(500).collect()),
        metadata: None,
    }
}

pub async fn send_text(
    sender: &SenderInfo,
    peer: &Device,
    text: &str,
    cancel: CancellationToken,
) -> Result<()> {
    ensure!(!text.is_empty(), "The share link is empty");
    let client = http_client(sender, peer)?;
    let files = HashMap::from([(FILE_ID.to_string(), text_file(text))]);
    let prepared = tokio::select! {
        result = client.prepare_upload(
            peer.protocol,
            &peer.host,
            peer.port,
            None,
            localsend::http::dto_v2::PrepareUploadRequestDtoV2 {
                info: sender.register(),
                files,
            },
            None,
            cancel.clone(),
        ) => result,
        _ = tokio::time::sleep(ACCEPT_TIMEOUT) => {
            bail!("{} did not respond. Is OmaSend or LocalSend open?", peer.alias);
        }
    };
    let prepared = prepared.map_err(|error| send_error(&peer.alias, error))?;
    let response = prepared
        .response
        .with_context(|| format!("{} accepted no files", peer.alias))?;
    let Some(token) = response.files.get(FILE_ID) else {
        bail!("{} did not accept the link", peer.alias);
    };
    let upload = client
        .upload(
            peer.protocol,
            &peer.host,
            peer.port,
            None,
            &response.session_id,
            FILE_ID,
            token,
            localsend::reqwest::Body::from(text.to_string()),
            cancel,
        )
        .await;
    if let Err(error) = upload {
        let _ = tokio::time::timeout(
            Duration::from_secs(3),
            client.cancel(peer.protocol, &peer.host, peer.port, &response.session_id),
        )
        .await;
        return Err(send_error(&peer.alias, error));
    }
    Ok(())
}

pub fn resolve_link(arg: Option<&str>, live: Option<&crate::live::LiveStatus>) -> Result<String> {
    if let Some(url) = arg.map(str::trim).filter(|url| !url.is_empty()) {
        ensure_share_url(url)?;
        return Ok(url.to_string());
    }
    let status = live.context(
        "No live share is running. Start sharing first, or pass the URL to --send-link.",
    )?;
    ensure!(
        status.stats.state != "ended",
        "The share has ended. Start a new share before sending its link."
    );
    ensure_share_url(&status.url)?;
    Ok(status.url.clone())
}

pub fn ensure_share_url(url: &str) -> Result<()> {
    ensure!(
        parse_share_url(url).is_some(),
        "That is not a browser share link."
    );
    Ok(())
}

pub fn parse_share_url(url: &str) -> Option<ShareUrl> {
    let url = url.trim();
    if url.len() > crate::live::status::MAX_URL_BYTES || url.contains([' ', '\\', '@', '\0']) {
        return None;
    }
    let rest = url.strip_prefix("http://")?;
    let (hostport, path) = rest.split_once('/')?;
    let path = format!("/{path}");
    let (host, port) = if let Some(inner) = hostport.strip_prefix('[') {
        let end = inner.find(']')?;
        let host = &inner[..end];
        let port = inner[end + 1..].strip_prefix(':')?;
        (host, port)
    } else {
        hostport.rsplit_once(':')?
    };
    if host.is_empty() {
        return None;
    }
    let port: u16 = port.parse().ok()?;
    if !(1..=65535).contains(&port) {
        return None;
    }
    let ip: std::net::IpAddr = host.parse().ok()?;
    if !allowed_share_ip(ip) {
        return None;
    }
    let token = path.strip_prefix("/s/")?.strip_suffix('/')?;
    if token.len() != 32 || !token.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    Some(ShareUrl {
        scheme: "http",
        host: host.to_string(),
        port: Some(port),
        path,
    })
}

fn allowed_share_ip(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(ip) => ip.is_loopback() || ip.is_private() || ip.is_link_local(),
        std::net::IpAddr::V6(ip) => {
            ip.is_loopback() || ip.is_unique_local() || ip.is_unicast_link_local()
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShareUrl {
    pub scheme: &'static str,
    pub host: String,
    pub port: Option<u16>,
    pub path: String,
}

impl ShareUrl {
    pub fn display_host(&self) -> String {
        match self.port {
            Some(port) if self.host.contains(':') => format!("[{}]:{port}", self.host),
            Some(port) => format!("{}:{port}", self.host),
            None if self.host.contains(':') => format!("[{}]", self.host),
            None => self.host.clone(),
        }
    }
}

pub fn spawn_window(url: &str) -> Result<()> {
    ensure_share_url(url)?;
    let exe = std::env::current_exe().context("failed to find omabeam")?;
    let mut cmd = Command::new(exe);
    cmd.arg("--send-link")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    cmd.spawn().context("failed to open the send window")?;
    Ok(())
}

fn http_client(sender: &SenderInfo, peer: &Device) -> Result<LsHttpClientV2> {
    match peer.protocol {
        ProtocolType::Https => LsHttpClientV2::try_new(
            &sender.private_key_pem,
            &sender.certificate_pem,
            Some(peer.fingerprint.clone()),
            None,
        )
        .context("Could not start a LocalSend connection"),
        ProtocolType::Http => {
            LsHttpClientV2::try_new_without_cert().context("Could not start a LocalSend connection")
        }
    }
}

fn device_from(device: &StatefulDevice) -> Option<Device> {
    let http = device.get_best_channel()?.http()?;
    Some(Device {
        fingerprint: device.device.fingerprint.clone(),
        alias: device.device.alias.clone(),
        model: device
            .device
            .device_model
            .clone()
            .unwrap_or_else(|| "LocalSend".into()),
        host: http.host.clone(),
        port: http.port,
        protocol: http.protocol,
    })
}

fn send_error(alias: &str, error: ClientError) -> anyhow::Error {
    if matches!(error, ClientError::Cancelled) {
        return anyhow::anyhow!("Send cancelled.");
    }
    if let ClientError::StatusCode(status) = &error {
        return anyhow::anyhow!(match status.status {
            401 => format!("{alias} requires a PIN. Copy the link instead."),
            403 => format!("{alias} declined the transfer."),
            409 => format!("{alias} is busy with another transfer. Try again."),
            429 => "Too many requests. Wait a moment and try again.".into(),
            _ => format!("Could not send to {alias}."),
        });
    }
    anyhow::anyhow!("Could not send to {alias}. Is OmaSend or LocalSend open?")
}

fn machine_name() -> String {
    let hostname = gethostname::gethostname()
        .to_string_lossy()
        .trim()
        .to_owned();
    if hostname.is_empty() {
        "OmaBeam".into()
    } else {
        hostname
    }
}

fn local_ipv4s() -> Vec<Ipv4Addr> {
    if_addrs::get_if_addrs()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|iface| match iface.ip() {
            IpAddr::V4(ip) if !ip.is_loopback() && !ip.is_link_local() => Some(ip),
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::live::{LiveStatus, StreamStats};
    use bytes::Bytes;
    use localsend::http::server::{
        ServerConfigV2, common::save::FileUploadTarget, start_with_port,
        v2::PrepareUploadDecisionV2, v2::ServerEventV2, web::WebConfig,
    };
    use localsend::http::state::ClientInfo;
    use std::sync::Mutex;
    use tokio::sync::mpsc;

    fn live(url: &str, state: &str) -> LiveStatus {
        LiveStatus {
            pid: 7,
            starttime: 1,
            url: url.into(),
            title: "Terminal".into(),
            stats: StreamStats {
                state: state.into(),
                ..StreamStats::default()
            },
        }
    }

    #[test]
    fn share_urls_reject_credentials_and_odd_schemes() {
        let url = "http://192.168.1.24:9847/s/0123456789abcdef0123456789abcdef/";
        assert!(parse_share_url(url).is_some());
        assert!(
            parse_share_url("https://example.com/s/0123456789abcdef0123456789abcdef/").is_none()
        );
        assert_eq!(
            parse_share_url("http://[::1]:9847/s/0123456789abcdef0123456789abcdef/")
                .unwrap()
                .display_host(),
            "[::1]:9847"
        );
        assert!(
            parse_share_url("http://8.8.8.8:9847/s/0123456789abcdef0123456789abcdef/").is_none()
        );
        assert!(parse_share_url("file:///tmp/test").is_none());
        assert!(parse_share_url("http://user:pass@host/").is_none());
        assert!(parse_share_url("http://host:0/").is_none());
        assert!(parse_share_url("http://host/has a space").is_none());
        assert!(parse_share_url("http://192.168.1.24:9847/s/abc/").is_none());
    }

    #[test]
    fn resolve_link_prefers_an_explicit_url_and_live_status() {
        let url = "http://192.168.1.24:9847/s/0123456789abcdef0123456789abcdef/";
        assert_eq!(resolve_link(Some(url), None).unwrap(), url);
        assert_eq!(resolve_link(None, Some(&live(url, "live"))).unwrap(), url);
        assert!(
            resolve_link(None, None)
                .unwrap_err()
                .to_string()
                .contains("No live share")
        );
        assert!(
            resolve_link(None, Some(&live(url, "ended")))
                .unwrap_err()
                .to_string()
                .contains("ended")
        );
        assert!(
            resolve_link(Some("javascript:alert(1)"), None)
                .unwrap_err()
                .to_string()
                .contains("browser share link")
        );
    }

    #[test]
    fn text_offer_uses_localsend_message_conventions() {
        let url = "http://192.168.1.24:9847/s/0123456789abcdef0123456789abcdef/";
        let file = text_file(url);
        assert_eq!(file.file_name, "Message.txt");
        assert_eq!(file.file_type, "text/plain");
        assert_eq!(file.size, url.len() as u64);
        assert_eq!(file.preview.as_deref(), Some(url));
    }

    #[test]
    fn declined_and_busy_peers_are_explained() {
        let declined = send_error(
            "Kitchen",
            ClientError::StatusCode(localsend::http::StatusCodeError {
                status: 403,
                message: None,
            }),
        );
        assert_eq!(declined.to_string(), "Kitchen declined the transfer.");
        let busy = send_error(
            "Kitchen",
            ClientError::StatusCode(localsend::http::StatusCodeError {
                status: 409,
                message: None,
            }),
        );
        assert_eq!(
            busy.to_string(),
            "Kitchen is busy with another transfer. Try again."
        );
    }

    struct TestPeer {
        port: u16,
        received: Arc<Mutex<Option<Vec<u8>>>>,
        _stop: oneshot::Sender<()>,
    }

    async fn start_peer(accept: bool) -> TestPeer {
        let received = Arc::new(Mutex::new(None));
        let (event_tx, mut event_rx) = mpsc::channel::<ServerEventV2>(16);
        let saved = received.clone();
        tokio::spawn(async move {
            while let Some(event) = event_rx.recv().await {
                match event {
                    ServerEventV2::PrepareUpload {
                        files, decision_tx, ..
                    } => {
                        let _ = decision_tx.send(if accept {
                            PrepareUploadDecisionV2::Accept(files.keys().cloned().collect())
                        } else {
                            PrepareUploadDecisionV2::Decline
                        });
                    }
                    ServerEventV2::FileUpload { target_tx, .. } => {
                        let (binary_tx, mut binary_rx) = mpsc::channel::<Bytes>(16);
                        let (result_tx, result_rx) = oneshot::channel();
                        let _ = target_tx.send(FileUploadTarget::Stream {
                            binary_tx,
                            result_rx,
                        });
                        let saved = saved.clone();
                        tokio::spawn(async move {
                            let mut bytes = Vec::new();
                            while let Some(chunk) = binary_rx.recv().await {
                                bytes.extend_from_slice(&chunk);
                            }
                            *saved.lock().unwrap() = Some(bytes);
                            let _ = result_tx.send(Ok(()));
                        });
                    }
                    _ => {}
                }
            }
        });
        let (stop, stop_rx) = oneshot::channel();
        let handle = start_with_port(
            0,
            None,
            ClientInfo {
                alias: "Kitchen".into(),
                version: "2.2".into(),
                device_model: Some("Test".into()),
                device_type: Some(DeviceType::Desktop),
                token: "receiver".into(),
            },
            None,
            Some(ServerConfigV2 {
                pin: None,
                verify_checksums: false,
                event_tx,
            }),
            WebConfig::default(),
            stop_rx,
        )
        .await
        .expect("test LocalSend peer");
        TestPeer {
            port: handle.port(),
            received,
            _stop: stop,
        }
    }

    fn test_sender() -> SenderInfo {
        SenderInfo {
            alias: "Studio".into(),
            fingerprint: "sender".into(),
            private_key_pem: String::new(),
            certificate_pem: String::new(),
        }
    }

    fn test_device(port: u16) -> Device {
        Device {
            fingerprint: "receiver".into(),
            alias: "Kitchen".into(),
            model: "Test".into(),
            host: "127.0.0.1".into(),
            port,
            protocol: ProtocolType::Http,
        }
    }

    #[tokio::test]
    async fn sends_a_share_link_over_localsend_http() {
        let peer = start_peer(true).await;
        let url = "http://192.168.1.24:9847/s/0123456789abcdef0123456789abcdef/";
        send_text(
            &test_sender(),
            &test_device(peer.port),
            url,
            CancellationToken::new(),
        )
        .await
        .unwrap();
        let received = peer.received.lock().unwrap().clone().unwrap();
        assert_eq!(received, url.as_bytes());
    }

    #[tokio::test]
    async fn declined_link_does_not_upload() {
        let peer = start_peer(false).await;
        let error = send_text(
            &test_sender(),
            &test_device(peer.port),
            "http://192.168.1.24:9847/s/0123456789abcdef0123456789abcdef/",
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("declined"));
        assert!(peer.received.lock().unwrap().is_none());
    }
}
