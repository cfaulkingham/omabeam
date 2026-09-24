//! In-process LocalSend client used to hand a live-share URL to another machine.
//!
//! OmaBeam speaks the protocol itself: it enumerates peers on the LAN
//! (multicast announcements, then a subnet probe of port 53317) and uploads the
//! link as a text message. The LocalSend app is not launched or required here.
//! The receiving computer still needs a LocalSend-compatible app (OmaSend or
//! LocalSend) open so it can accept the transfer.

use anyhow::{Context, Result, bail, ensure};
use localsend::{
    crypto::cert::{generate_self_signed, identity_fingerprint},
    discovery::{self, DeviceIdentity, DiscoveryConfig, DiscoveryHandle, StatefulDevice},
    http::{
        client::{ClientError, LsHttpClientV2},
        dto_v2::RegisterDtoV2,
    },
    model::{
        discovery::{DeviceType, PROTOCOL_VERSION_V2, ProtocolType},
        transfer::FileDto,
    },
    multicast::{
        DEFAULT_MULTICAST_GROUP, DEFAULT_MULTICAST_GROUP_V6, DEFAULT_PORT, MulticastDevice,
    },
};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    ffi::OsString,
    fs::{File, OpenOptions},
    io::{Read, Write},
    net::{IpAddr, Ipv4Addr},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

const FILE_ID: &str = "0";
const ACCEPT_TIMEOUT: Duration = Duration::from_secs(300);
/// The link is a few hundred bytes, so an accepted upload that runs this long
/// is stuck on the receiver's side.
#[cfg(not(test))]
const UPLOAD_TIMEOUT: Duration = Duration::from_secs(30);
#[cfg(test)]
const UPLOAD_TIMEOUT: Duration = Duration::from_secs(1);
const SCAN_GRACE: Duration = Duration::from_secs(2);
const IDENTITY_FILE: &str = "localsend-identity.json";
const IDENTITY_VERSION: u32 = 1;
const IDENTITY_MAX_BYTES: u64 = 16 * 1024;

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
    /// This machine's LocalSend identity, kept in
    /// `$XDG_CONFIG_HOME/omabeam/localsend-identity.json` (default
    /// `~/.config/omabeam`) and created there on first use.
    pub fn load_or_create() -> Result<Self> {
        let identity = match identity_path() {
            Ok(path) => Identity::load_or_create_at(&path)?,
            // Without a config directory a fresh identity still sends.
            Err(_) => Identity::generate()?,
        };
        Ok(Self::new(machine_name(), identity))
    }

    fn new(alias: String, identity: Identity) -> Self {
        Self {
            alias,
            fingerprint: identity.fingerprint,
            private_key_pem: identity.private_key_pem,
            certificate_pem: identity.certificate_pem,
        }
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

/// The certificate OmaBeam presents to receivers. Kept across send windows so
/// receivers see one fingerprint, and no window waits for a new key.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Identity {
    fingerprint: String,
    private_key_pem: String,
    certificate_pem: String,
}

/// The file on disk: a format version beside the identity.
#[derive(Serialize, Deserialize)]
struct IdentityFile<I> {
    version: u32,
    #[serde(flatten)]
    identity: I,
}

impl Identity {
    fn generate() -> Result<Self> {
        let cert = generate_self_signed().context("Could not create a LocalSend identity")?;
        Ok(Self {
            fingerprint: cert.fingerprint,
            private_key_pem: cert.private_key_pem,
            certificate_pem: cert.certificate_pem,
        })
    }

    /// The identity stored at `path`, or a new one saved there when the file
    /// is missing or invalid.
    fn load_or_create_at(path: &Path) -> Result<Self> {
        if let Some(identity) = read_identity(path) {
            return Ok(identity);
        }
        let identity = Self::generate()?;
        // An identity that could not be saved still sends this time.
        let _ = save_identity(path, &identity);
        Ok(identity)
    }
}

/// The stored identity, or none if the file is missing, unreadable, oversized,
/// malformed, from another format version, or not a usable key pair.
fn read_identity(path: &Path) -> Option<Identity> {
    let mut bytes = Vec::new();
    File::open(path)
        .ok()?
        .take(IDENTITY_MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() as u64 > IDENTITY_MAX_BYTES {
        return None;
    }
    let file: IdentityFile<Identity> = serde_json::from_slice(&bytes).ok()?;
    if file.version != IDENTITY_VERSION {
        return None;
    }
    let identity = file.identity;
    let fingerprint =
        identity_fingerprint(&identity.certificate_pem, &identity.private_key_pem).ok()?;
    (fingerprint == identity.fingerprint).then_some(identity)
}

/// Replace the file atomically through a private temporary file, the way
/// `live::prefs` saves the picker's settings. The file holds a private key.
fn save_identity(path: &Path, identity: &Identity) -> Result<()> {
    let dir = path.parent().context("identity path has no directory")?;
    let bytes = serde_json::to_vec_pretty(&IdentityFile {
        version: IDENTITY_VERSION,
        identity,
    })?;
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    let tmp = dir.join(format!(
        ".{IDENTITY_FILE}.{}.{nanos}.tmp",
        std::process::id()
    ));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)
        .with_context(|| format!("create {}", tmp.display()))?;
    let written = file
        .write_all(&bytes)
        .and_then(|()| file.sync_all())
        .and_then(|()| std::fs::rename(&tmp, path));
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written.with_context(|| format!("write {}", path.display()))
}

fn identity_path() -> Result<PathBuf> {
    identity_path_from(
        std::env::var_os("XDG_CONFIG_HOME"),
        std::env::var_os("HOME"),
    )
}

/// Beside the picker's saved settings in `$XDG_CONFIG_HOME/omabeam`.
fn identity_path_from(
    xdg_config_home: Option<OsString>,
    home: Option<OsString>,
) -> Result<PathBuf> {
    let base = xdg_config_home
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            home.filter(|value| !value.is_empty())
                .map(|home| PathBuf::from(home).join(".config"))
        })
        .context("HOME or XDG_CONFIG_HOME is required for the LocalSend identity")?;
    ensure!(base.is_absolute(), "config directory must be absolute");
    Ok(base.join("omabeam").join(IDENTITY_FILE))
}

/// A receiver answered 401: it wants a PIN, or rejected the one sent. The
/// protocol answers both with 401, so which it was follows from whether a PIN
/// was sent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PinRejected {
    pub alias: String,
    pub pin_sent: bool,
}

impl std::fmt::Display for PinRejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.pin_sent {
            write!(f, "Incorrect PIN for {}.", self.alias)
        } else {
            write!(f, "{} requires a PIN.", self.alias)
        }
    }
}

impl std::error::Error for PinRejected {}

pub struct Discovery {
    handle: Arc<DiscoveryHandle>,
    stop: Option<oneshot::Sender<()>>,
    scanning: Arc<AtomicBool>,
    multicast_error: Option<String>,
}

/// Discovery as a device that cannot receive: OmaBeam runs no LocalSend
/// server, so it confirms peers with info requests and never registers with
/// them, which would make them list OmaBeam at a port nobody serves.
fn discovery_config(info: &SenderInfo, multicast_port: u16) -> DiscoveryConfig {
    DiscoveryConfig {
        group: DEFAULT_MULTICAST_GROUP,
        group_v6: Some(DEFAULT_MULTICAST_GROUP_V6),
        port: multicast_port,
        interface_filter: Default::default(),
        // Never sent: OmaBeam does not announce itself, and info requests
        // carry nothing about it. The fingerprint filters out its own echoes.
        device: MulticastDevice {
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
        receivable: false,
    }
}

impl Discovery {
    pub async fn start(info: &SenderInfo) -> Result<Self> {
        let (stop_tx, stop_rx) = oneshot::channel();
        let handle = discovery::start(discovery_config(info, DEFAULT_PORT), stop_rx).await;
        // Announcements stay answered (the default): for a device that cannot
        // receive, the answer is a pinned `GET /info` that registers nothing,
        // and it finds LocalSend apps opened after the one subnet scan.
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

/// Offers `text` to `peer` as a message and uploads it once accepted. `pin`
/// answers a receiver that requires one; a 401 comes back as [`PinRejected`].
pub async fn send_text(
    sender: &SenderInfo,
    peer: &Device,
    text: &str,
    pin: Option<&str>,
    cancel: CancellationToken,
) -> Result<()> {
    ensure!(!text.is_empty(), "The share link is empty");
    let client = http_client(sender, peer)?;
    let file = text_file(text);
    let previewed = file.preview.as_deref() == Some(text);
    let files = HashMap::from([(FILE_ID.to_string(), file)]);
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
            pin,
            cancel.clone(),
        ) => result,
        _ = tokio::time::sleep(ACCEPT_TIMEOUT) => {
            bail!("{} did not respond. Is OmaSend or LocalSend open?", peer.alias);
        }
    };
    let prepared = prepared.map_err(|error| match error {
        ClientError::StatusCode(status) if status.status == 401 => PinRejected {
            alias: peer.alias.clone(),
            pin_sent: pin.is_some(),
        }
        .into(),
        error => send_error(&peer.alias, error),
    })?;
    let Some(response) = prepared.response else {
        // 204, "no file transfer needed": the receiver took the message from
        // the offer's preview, which delivered it only if it held all of it.
        ensure!(previewed, "{} accepted no files", peer.alias);
        return Ok(());
    };
    let Some(token) = response.files.get(FILE_ID) else {
        bail!("{} did not accept the link", peer.alias);
    };
    let upload = tokio::select! {
        result = client.upload(
            peer.protocol,
            &peer.host,
            peer.port,
            None,
            &response.session_id,
            FILE_ID,
            token,
            localsend::reqwest::Body::from(text.to_string()),
            cancel,
        ) => result.map_err(|error| send_error(&peer.alias, error)),
        _ = tokio::time::sleep(UPLOAD_TIMEOUT) => {
            Err(anyhow::anyhow!("The device did not finish receiving the link."))
        }
    };
    if upload.is_err() {
        // Frees the receiver's session, which would block the next send.
        let _ = tokio::time::timeout(
            Duration::from_secs(3),
            client.cancel(peer.protocol, &peer.host, peer.port, &response.session_id),
        )
        .await;
    }
    upload
}

pub fn resolve_link(live: Option<&crate::live::LiveStatus>) -> Result<String> {
    let status = live.context("No live share is running. Start sharing first.")?;
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
            401 => format!("{alias} requires a PIN."),
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
        v2::PrepareUploadDecisionV2, v2::ServerEventV2, v2::SessionEndReasonV2, web::WebConfig,
    };
    use localsend::http::state::ClientInfo;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;
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
    fn resolve_link_returns_live_status_url_or_error() {
        let url = "http://192.168.1.24:9847/s/0123456789abcdef0123456789abcdef/";
        assert_eq!(resolve_link(Some(&live(url, "live"))).unwrap(), url);
        let err = resolve_link(None).unwrap_err().to_string();
        assert!(err.contains("No live share") && err.contains("Start sharing first"));
        assert!(
            resolve_link(Some(&live(url, "ended")))
                .unwrap_err()
                .to_string()
                .contains("ended")
        );
        assert!(
            resolve_link(Some(&live("javascript:alert(1)", "live")))
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
        /// The previews of the files each prepare-upload offered.
        previews: Arc<Mutex<Vec<String>>>,
        /// Aliases of the devices that registered with this peer.
        registered: Arc<Mutex<Vec<String>>>,
        /// Cancel requests the sender made.
        cancelled: Arc<Mutex<usize>>,
        _stop: oneshot::Sender<()>,
    }

    impl TestPeer {
        /// The devices that registered so far. A register request of the
        /// test's own marks the end: the server queues its events in order,
        /// so every earlier registration is recorded before this one.
        async fn registrations(&self) -> Vec<String> {
            LsHttpClientV2::try_new_without_cert()
                .unwrap()
                .register(
                    ProtocolType::Http,
                    "127.0.0.1",
                    self.port,
                    RegisterDtoV2 {
                        alias: "end-of-test".into(),
                        version: PROTOCOL_VERSION_V2.into(),
                        device_model: None,
                        device_type: None,
                        fingerprint: "end-of-test".into(),
                        port: 1,
                        protocol: ProtocolType::Http,
                        download: false,
                    },
                )
                .await
                .expect("register with the test peer");
            wait_until(|| {
                self.registered
                    .lock()
                    .unwrap()
                    .contains(&"end-of-test".into())
            })
            .await;
            let mut registered = self.registered.lock().unwrap().clone();
            registered.retain(|alias| alias != "end-of-test");
            registered
        }
    }

    async fn wait_until(done: impl Fn() -> bool) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while !done() {
            assert!(tokio::time::Instant::now() < deadline, "timed out");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    #[derive(Default)]
    struct PeerOptions {
        decline: bool,
        pin: Option<&'static str>,
        /// Accept the upload request but never finish receiving it.
        stall_upload: bool,
        /// Take a message from its preview: accept, but request no upload,
        /// which the server answers with 204 (no file transfer needed).
        message_only: bool,
    }

    async fn start_peer(accept: bool) -> TestPeer {
        start_peer_with(PeerOptions {
            decline: !accept,
            ..PeerOptions::default()
        })
        .await
    }

    async fn start_peer_with(options: PeerOptions) -> TestPeer {
        let received = Arc::new(Mutex::new(None));
        let previews = Arc::new(Mutex::new(Vec::new()));
        let registered = Arc::new(Mutex::new(Vec::new()));
        let cancelled = Arc::new(Mutex::new(0));
        let (event_tx, mut event_rx) = mpsc::channel::<ServerEventV2>(16);
        let saved = received.clone();
        let offered = previews.clone();
        let registrations = registered.clone();
        let cancellations = cancelled.clone();
        tokio::spawn(async move {
            let mut stalled = Vec::new();
            while let Some(event) = event_rx.recv().await {
                match event {
                    ServerEventV2::PrepareUpload {
                        files, decision_tx, ..
                    } => {
                        offered
                            .lock()
                            .unwrap()
                            .extend(files.values().filter_map(|file| file.preview.clone()));
                        let _ = decision_tx.send(if options.decline {
                            PrepareUploadDecisionV2::Decline
                        } else if options.message_only {
                            PrepareUploadDecisionV2::Accept(Default::default())
                        } else {
                            PrepareUploadDecisionV2::Accept(files.keys().cloned().collect())
                        });
                    }
                    ServerEventV2::FileUpload { target_tx, .. } if options.stall_upload => {
                        // Held, never answered: the upload request hangs.
                        stalled.push(target_tx);
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
                    ServerEventV2::Register { info, .. } => {
                        registrations.lock().unwrap().push(info.alias);
                    }
                    // A cancel for a session the server already ended (e.g.
                    // on a dropped upload) arrives as `CancelReceived`.
                    ServerEventV2::SessionEnd {
                        reason: SessionEndReasonV2::Cancelled,
                        ..
                    }
                    | ServerEventV2::CancelReceived { .. } => *cancellations.lock().unwrap() += 1,
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
                pin: options.pin.map(Into::into),
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
            previews,
            registered,
            cancelled,
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
            None,
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
            None,
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("declined"));
        assert!(peer.received.lock().unwrap().is_none());
    }

    const URL: &str = "http://192.168.1.24:9847/s/0123456789abcdef0123456789abcdef/";

    fn generated_sender() -> SenderInfo {
        SenderInfo::new("Studio".into(), Identity::generate().unwrap())
    }

    #[tokio::test]
    async fn discovery_confirms_peers_without_registering_with_them() {
        let peer = start_peer(true).await;
        let (_stop, stop_rx) = oneshot::channel();
        // Port 0: the test's multicast sockets stay off the LocalSend port.
        let handle = discovery::start(discovery_config(&generated_sender(), 0), stop_rx).await;

        handle
            .discover("127.0.0.1", peer.port, ProtocolType::Http)
            .await
            .unwrap()
            .expect("the peer answers");
        handle
            .scan_subnet(Ipv4Addr::new(127, 0, 0, 99), peer.port, ProtocolType::Http)
            .await
            .unwrap();

        let devices: Vec<Device> = handle.devices().iter().filter_map(device_from).collect();
        assert!(
            devices
                .iter()
                .any(|device| device.alias == "Kitchen" && device.port == peer.port),
            "{devices:?}"
        );
        assert_eq!(peer.registrations().await, Vec::<String>::new());
    }

    #[tokio::test]
    async fn pin_protected_peers_receive_the_link_with_the_right_pin() {
        let peer = start_peer_with(PeerOptions {
            pin: Some("1234"),
            ..PeerOptions::default()
        })
        .await;
        let sender = test_sender();
        let device = test_device(peer.port);
        let send = |pin| send_text(&sender, &device, URL, pin, CancellationToken::new());

        let error = send(None).await.unwrap_err();
        assert!(error.to_string().contains("requires a PIN"), "{error}");
        assert_eq!(
            error.downcast_ref::<PinRejected>(),
            Some(&PinRejected {
                alias: "Kitchen".into(),
                pin_sent: false,
            })
        );

        let error = send(Some("0000")).await.unwrap_err();
        assert_eq!(error.to_string(), "Incorrect PIN for Kitchen.");
        assert!(
            error
                .downcast_ref::<PinRejected>()
                .is_some_and(|pin| pin.pin_sent)
        );
        assert!(peer.received.lock().unwrap().is_none());

        send(Some("1234")).await.unwrap();
        assert_eq!(
            peer.received.lock().unwrap().as_deref(),
            Some(URL.as_bytes())
        );
    }

    #[tokio::test]
    async fn a_message_taken_from_its_preview_counts_as_sent() {
        let peer = start_peer_with(PeerOptions {
            message_only: true,
            ..PeerOptions::default()
        })
        .await;
        let sender = test_sender();
        let device = test_device(peer.port);

        // 204, "no file transfer needed": the preview carried the whole link.
        send_text(&sender, &device, URL, None, CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(*peer.previews.lock().unwrap(), [URL]);
        assert!(
            peer.received.lock().unwrap().is_none(),
            "nothing is uploaded"
        );

        // A text longer than its preview did not arrive whole.
        let long = format!("{URL}{}", "x".repeat(500));
        let error = send_text(&sender, &device, &long, None, CancellationToken::new())
            .await
            .unwrap_err();
        assert_eq!(error.to_string(), "Kitchen accepted no files");
    }

    #[tokio::test]
    async fn an_unfinished_upload_times_out_and_is_cancelled() {
        let peer = start_peer_with(PeerOptions {
            stall_upload: true,
            ..PeerOptions::default()
        })
        .await;
        let started = std::time::Instant::now();
        let error = send_text(
            &test_sender(),
            &test_device(peer.port),
            URL,
            None,
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
        let elapsed = started.elapsed();

        assert_eq!(
            error.to_string(),
            "The device did not finish receiving the link."
        );
        assert!(
            elapsed >= UPLOAD_TIMEOUT && elapsed < UPLOAD_TIMEOUT + Duration::from_secs(3),
            "{elapsed:?}"
        );
        // The receiver is told, so a session it still holds cannot block the
        // next send.
        wait_until(|| *peer.cancelled.lock().unwrap() == 1).await;
    }

    fn file_names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn identity_is_saved_privately_and_reused() {
        let home = tempfile::TempDir::new().unwrap();
        let dir = home.path().join("omabeam");
        let path = dir.join(IDENTITY_FILE);

        let created = Identity::load_or_create_at(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        let saved: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            saved,
            serde_json::json!({
                "version": 1,
                "fingerprint": created.fingerprint,
                "private_key_pem": created.private_key_pem,
                "certificate_pem": created.certificate_pem,
            })
        );

        assert_eq!(Identity::load_or_create_at(&path).unwrap(), created);
        assert_eq!(file_names(&dir), [IDENTITY_FILE]);
    }

    #[test]
    fn missing_or_invalid_identities_are_replaced() {
        let home = tempfile::TempDir::new().unwrap();
        let dir = home.path().join("omabeam");
        let path = dir.join(IDENTITY_FILE);
        let valid = Identity::load_or_create_at(&path).unwrap();
        let other = Identity::generate().unwrap();
        let file = |version: u32, identity: &Identity| {
            serde_json::to_string(&IdentityFile { version, identity }).unwrap()
        };

        for text in [
            "not json".to_string(),
            file(2, &valid),
            file(
                1,
                &Identity {
                    fingerprint: other.fingerprint.clone(),
                    ..valid.clone()
                },
            ),
            file(
                1,
                &Identity {
                    private_key_pem: other.private_key_pem.clone(),
                    ..valid.clone()
                },
            ),
            format!(
                "{:width$}",
                file(1, &valid),
                width = IDENTITY_MAX_BYTES as usize + 1
            ),
        ] {
            std::fs::write(&path, &text).unwrap();
            let replaced = Identity::load_or_create_at(&path).unwrap();
            assert_ne!(replaced, valid, "{text}");
            assert_eq!(
                Identity::load_or_create_at(&path).unwrap(),
                replaced,
                "{text}"
            );
        }
        assert_eq!(file_names(&dir), [IDENTITY_FILE]);
    }

    #[test]
    fn identity_lives_in_the_omabeam_config_directory() {
        let path = |xdg: Option<&str>, home: Option<&str>| {
            identity_path_from(xdg.map(Into::into), home.map(Into::into))
        };
        assert_eq!(
            path(Some("/xdg"), Some("/home/me")).unwrap(),
            Path::new("/xdg/omabeam/localsend-identity.json")
        );
        assert_eq!(
            path(Some(""), Some("/home/me")).unwrap(),
            Path::new("/home/me/.config/omabeam/localsend-identity.json")
        );
        assert!(path(Some("relative"), Some("/home/me")).is_err());
        assert!(path(None, None).is_err());
    }
}
