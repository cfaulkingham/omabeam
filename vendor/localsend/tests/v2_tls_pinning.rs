#![cfg(feature = "http")]

//! Verifies that the client authenticates the peer certificate during the TLS
//! handshake, i.e. before any request data is written.

use bytes::Bytes;
use futures_util::StreamExt;
use localsend::http::client::{ClientError, LsHttpClientV2};
use localsend::http::dto_v2::{PrepareUploadRequestDtoV2, RegisterDtoV2};
use localsend::http::server::common::save::FileUploadTarget;
use localsend::http::server::v2::{PrepareUploadDecisionV2, ServerEventV2};
use localsend::http::server::web::{WebConfig, WebMode};
use localsend::http::server::{start_with_port, ServerConfigV2, TlsConfig};
use localsend::http::state::ClientInfo;
use localsend::model::discovery::ProtocolType;
use localsend::model::transfer::FileDto;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::sync::CancellationToken;

/// A generated self-signed certificate, as LocalSend peers use them.
struct Identity {
    cert: String,
    private_key: String,
    /// Uppercase-hex SHA-256 of the certificate in DER format.
    fingerprint: String,
}

fn generate_identity() -> Identity {
    let cert = localsend::crypto::cert::generate_self_signed().unwrap();

    Identity {
        cert: cert.certificate_pem,
        private_key: cert.private_key_pem,
        fingerprint: cert.fingerprint,
    }
}

/// A test-only RSA-2048 identity like the ones LocalSend apps generate
/// (`CN=LocalSend User`, no extensions), valid from 2000 to 2126. Generated
/// identities are ECDSA, so this keeps RSA peers covered.
const RSA_CERT: &str = "-----BEGIN CERTIFICATE-----
MIICwDCCAaigAwIBAgIURjN9PVLn4ChGSf9VxdTervPTYycwDQYJKoZIhvcNAQEL
BQAwGTEXMBUGA1UEAwwOTG9jYWxTZW5kIFVzZXIwIBcNMDAwMTAxMDAwMDAwWhgP
MjEyNjAxMDEwMDAwMDBaMBkxFzAVBgNVBAMMDkxvY2FsU2VuZCBVc2VyMIIBIjAN
BgkqhkiG9w0BAQEFAAOCAQ8AMIIBCgKCAQEA8r092IiIOQxJDaM2allA4jJTzRGn
w3xhyM8//VX+kNRGgLhvdliMqOEi/8ciGWDql1q0lgW+xTk/HWpZGajtUTcCXtVI
XmhBTzJ00HJssQe8bzaM4WufrDtnsCmjSv9O3Pp1jQ3ID5OiK4Ap8osRPVCQG3kb
k0zpB5q/he+z+izUoq4Bxs8gGXtHP2sAeSQ3AVXAR48Ol6XgYNPr4M9iX22rxEQe
f7oqsSdQ5ntM/qVd0Fve2f6PHAStMOUDBqrrr0d2M8yDjv9l8HvjSYdYA7nkbMGQ
Z8zY5gohRuVxMF3Og5thgJoVdZ0NFrywgrz/o7eaRKsRllduu+0EeDsPuQIDAQAB
MA0GCSqGSIb3DQEBCwUAA4IBAQDvVV7GIIBqaJnYl7h13ATxq7ekLEASO6Gnm8G2
GvOrZ1+9MjhTJEL29vazdDVFECjMGt/IIBBDKlOVLN+5gSaB69GDD1Xmgu596BTy
2pHRt5Au9JyqhKex3gbcGLX7Gfi98wEiR2GpYqCSEV6r7T04NCSzR/bq5NL14nfl
AscWdquE2zA7CthtFDudAatMuHTGyKesp5H7lpqF2qbUQ9LfcVMD+8ZiO1J6H4M4
r6HpM4wUYxqOwltSIxPaHzkSlHD1eCSUSwjgrdI4ODMcgCru6YAJ9YvsvuKBCj7P
oKV9a/+Mf2m6D/o/woKHF8xGSzJLHft/5bXWjt7TcAwAMUFm
-----END CERTIFICATE-----";

/// The private key of [RSA_CERT]; it protects nothing outside this test.
const RSA_KEY: &str = "-----BEGIN PRIVATE KEY-----
MIIEvwIBADANBgkqhkiG9w0BAQEFAASCBKkwggSlAgEAAoIBAQDyvT3YiIg5DEkN
ozZqWUDiMlPNEafDfGHIzz/9Vf6Q1EaAuG92WIyo4SL/xyIZYOqXWrSWBb7FOT8d
alkZqO1RNwJe1UheaEFPMnTQcmyxB7xvNozha5+sO2ewKaNK/07c+nWNDcgPk6Ir
gCnyixE9UJAbeRuTTOkHmr+F77P6LNSirgHGzyAZe0c/awB5JDcBVcBHjw6XpeBg
0+vgz2JfbavERB5/uiqxJ1Dme0z+pV3QW97Z/o8cBK0w5QMGquuvR3YzzIOO/2Xw
e+NJh1gDueRswZBnzNjmCiFG5XEwXc6Dm2GAmhV1nQ0WvLCCvP+jt5pEqxGWV267
7QR4Ow+5AgMBAAECggEANUp+rW8sGPotf6T5aVe2+vH5m+foYO6JuUBm2ANtI/a2
k8PqFJ1UJC9pM6ZQQioY1qm/Q8B3JW1wAdE7RnrjvxTInHFOtgtFa1G0e57jZepA
4FmljEfIglN4bCB4DRsIAVOrQxBnf6P10n8Mtob5ffCV8rIf77LO5jB/I7SAWsfX
XW2wBWAPj6jFmFhYHiDE12u034Kgc9OXC8MHJBlzKmJbkhKG1IEYbzrO5uB4rLzh
UrvWN6sqPUrI1oDBQlqUsKALTe1j+WyxMcY0CoJ8PQJ4yvYS0oIidfD6c7HKV/wO
EZuO86Nm55QeTZuTtt89eY/6SesQrb0dMuPqybOgQQKBgQD95YlAIkDPwHoehS6a
r1GNBQb9F3fvLDYzgV3zbKQhS/+70Ldl/KVfc9Nzho//rfDzUnqDpK/OrEbiedu6
3/qgMrFWFmAj/y2Jfi4OfH+izvS4HeFh03RkRaOAjUoKUN2zIpQ2iRDAqm54qsRH
iftbtnXiNdfkpvoyIPWOl4iQnQKBgQD0wAr3iRUqRsdq5WbgzExQBdWyQ2Lrfic+
bY0e1IXwOrA7vX54edSU3A0VWxc43inz7b1Tvn/lghXKsSwEaamxarWnuw2uy6mI
NUAiXS0qbswLZuFcMHn1qD1XjMPuxudo7NlS+qBGJpUIHTjwJuS4n4NFA0Vt1AC7
7dw+VgOqzQKBgQDMlVBq5S7kOGwue5J6AhrsTsi611ZE2pMyHWPe6gO28M44QI4H
F/UJXQNda2hM1ZP5/NPLj8A6AId9pkNv6VrCCxbqD02Ys921lH74HUmbnreCRFF/
yitXQOXmh/LJLS/3UD2tU/SzyqEJilBudLidxgBnLouwKvwfuH8lTuUz4QKBgQCT
2LKHR3rOmAT80OjtoNv0dyHY1pYZ1ZIVRw45cn4aWQl+fQC5/DZrWeZZYTZM9v92
x8VXojsWRBev4ojJfZ6xetmTIg6o6YRWsHw2hfyzpkyCErrN+sahm+i6S2FMi0Dd
BD5fvuA0oDfkt+peP9BoqosclFZLewaaf5xM157NfQKBgQDWI6Ad8bZBm1mAyJf7
BaF7H0uwyqIPJX506diQTrFmlQdyZ8hOr7zx75hdFWAHiDWylannqoor1L07dYu+
77NyQCSelLPpF1psbvQ1yDyVRzxjRKfOs7GzNVIya9iUGzL5qPqB+uVHci5Ijcrb
rfxZdbayNo8PVBxpMZG5KCSm9w==
-----END PRIVATE KEY-----";

fn rsa_identity() -> Identity {
    use rustls::pki_types::pem::PemObject;
    let der = rustls::pki_types::CertificateDer::from_pem_slice(RSA_CERT.as_bytes()).unwrap();

    Identity {
        cert: RSA_CERT.to_string(),
        private_key: RSA_KEY.to_string(),
        fingerprint: localsend::crypto::cert::fingerprint_from_cert_der(&der),
    }
}

struct TestServer {
    port: u16,
    /// Requests that reached the application layer.
    prepare_uploads: Arc<Mutex<Vec<String>>>,
    /// Uploaded file contents, mapped by file ID.
    received: Arc<Mutex<Vec<(String, Vec<u8>)>>>,
    _stop_tx: oneshot::Sender<()>,
}

/// Starts a test server over TLS using the given identity.
async fn start_tls_server(identity: &Identity) -> TestServer {
    start_tls_server_with_web(identity, WebConfig::default()).await
}

/// Starts a test server over TLS, optionally with the web pages enabled
/// (which makes the client certificate optional).
async fn start_tls_server_with_web(identity: &Identity, web: WebConfig) -> TestServer {
    let _ = tracing_subscriber::fmt().with_test_writer().try_init();
    let prepare_uploads: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let received: Arc<Mutex<Vec<(String, Vec<u8>)>>> = Arc::new(Mutex::new(Vec::new()));

    let (event_tx, mut event_rx) = mpsc::channel::<ServerEventV2>(16);

    tokio::spawn({
        let prepare_uploads = prepare_uploads.clone();
        let received = received.clone();
        async move {
            while let Some(event) = event_rx.recv().await {
                match event {
                    ServerEventV2::PrepareUpload {
                        files, decision_tx, ..
                    } => {
                        prepare_uploads
                            .lock()
                            .await
                            .extend(files.keys().cloned().collect::<Vec<_>>());
                        let _ = decision_tx.send(PrepareUploadDecisionV2::Accept(
                            files.keys().cloned().collect(),
                        ));
                    }
                    ServerEventV2::FileUpload {
                        file_id, target_tx, ..
                    } => {
                        let received = received.clone();
                        let (binary_tx, mut binary_rx) = mpsc::channel(16);
                        let (result_tx, result_rx) = oneshot::channel();
                        let _ = target_tx.send(FileUploadTarget::Stream {
                            binary_tx,
                            result_rx,
                        });
                        tokio::spawn(async move {
                            let mut bytes = Vec::new();
                            while let Some(chunk) = binary_rx.recv().await {
                                bytes.extend_from_slice(&chunk);
                            }
                            received.lock().await.push((file_id, bytes));
                            let _ = result_tx.send(Ok(()));
                        });
                    }
                    _ => {}
                }
            }
        }
    });

    let (stop_tx, stop_rx) = oneshot::channel::<()>();

    // Port 0 lets the OS pick a free port, avoiding collisions between tests.
    let handle = start_with_port(
        0,
        Some(TlsConfig {
            cert: identity.cert.clone(),
            private_key: identity.private_key.clone(),
        }),
        ClientInfo {
            alias: "Test Server".to_string(),
            version: "2.2".to_string(),
            device_model: Some("Rust".to_string()),
            device_type: None,
            token: identity.fingerprint.clone(),
        },
        None,
        Some(ServerConfigV2 {
            pin: None,
            verify_checksums: true,
            event_tx,
        }),
        web,
        stop_rx,
    )
    .await
    .expect("Failed to start server");

    TestServer {
        port: handle.port(),
        prepare_uploads,
        received,
        _stop_tx: stop_tx,
    }
}

fn client(sender: &Identity, expected_fingerprint: Option<&str>) -> LsHttpClientV2 {
    LsHttpClientV2::try_new(
        &sender.private_key,
        &sender.cert,
        expected_fingerprint.map(|f| f.to_string()),
        None,
    )
    .unwrap()
}

fn sender_info(fingerprint: &str) -> RegisterDtoV2 {
    RegisterDtoV2 {
        alias: "Test Sender".to_string(),
        version: "2.2".to_string(),
        device_model: Some("Rust".to_string()),
        device_type: None,
        fingerprint: fingerprint.to_string(),
        port: 53317,
        protocol: ProtocolType::Https,
        download: false,
    }
}

fn file_dto(id: &str, size: u64) -> FileDto {
    FileDto {
        id: id.to_string(),
        file_name: format!("{id}.bin"),
        size,
        file_type: "application/octet-stream".to_string(),
        sha256: None,
        preview: None,
        metadata: None,
    }
}

fn prepare_upload_request(sender: &Identity, files: &[FileDto]) -> PrepareUploadRequestDtoV2 {
    PrepareUploadRequestDtoV2 {
        info: sender_info(&sender.fingerprint),
        files: files
            .iter()
            .map(|file| (file.id.clone(), file.clone()))
            .collect(),
    }
}

/// Uploads `bytes` and returns how many bytes were actually read from the
/// source stream, so a test can assert that nothing left the machine.
async fn upload_bytes(
    client: &LsHttpClientV2,
    port: u16,
    session_id: &str,
    file_id: &str,
    token: &str,
    bytes: &[u8],
) -> (Result<(), ClientError>, u64) {
    let (tx, rx) = mpsc::channel::<Bytes>(4);
    let chunks: Vec<Vec<u8>> = bytes.chunks(1024).map(|chunk| chunk.to_vec()).collect();
    tokio::spawn(async move {
        for chunk in chunks {
            if tx.send(Bytes::from(chunk)).await.is_err() {
                break;
            }
        }
    });

    let sent = Arc::new(AtomicU64::new(0));
    let progress = sent.clone();
    let body =
        localsend::reqwest::Body::wrap_stream(ReceiverStream::new(rx).map(move |chunk: Bytes| {
            progress.fetch_add(chunk.len() as u64, Ordering::Relaxed);
            Ok::<Bytes, std::io::Error>(chunk)
        }));

    let result = client
        .upload(
            ProtocolType::Https,
            "127.0.0.1",
            port,
            None,
            session_id,
            file_id,
            token,
            body,
            CancellationToken::new(),
        )
        .await;

    (result, sent.load(Ordering::Relaxed))
}

/// The happy path: the pinned fingerprint matches, so the transfer works.
/// This also covers that mTLS still functions with the hand-built TLS config.
#[tokio::test]
async fn test_transfer_with_matching_fingerprint() {
    let server_identity = generate_identity();
    let sender = generate_identity();
    let server = start_tls_server(&server_identity).await;
    let client = client(&sender, Some(&server_identity.fingerprint));

    let content = b"hello localsend".repeat(100);
    let files = vec![file_dto("file-1", content.len() as u64)];

    let result = client
        .prepare_upload(
            ProtocolType::Https,
            "127.0.0.1",
            server.port,
            None,
            prepare_upload_request(&sender, &files),
            None,
            CancellationToken::new(),
        )
        .await
        .expect("prepare-upload should succeed");

    let response = result.response.expect("expected a session");
    let token = response.files.get("file-1").unwrap();

    let (upload_result, sent) = upload_bytes(
        &client,
        server.port,
        &response.session_id,
        "file-1",
        token,
        &content,
    )
    .await;

    upload_result.expect("upload should succeed");
    assert_eq!(sent, content.len() as u64);

    let received = server.received.lock().await;
    assert_eq!(received.len(), 1);
    assert_eq!(received[0].1, content);
}

/// The peer presents a valid self-signed certificate, but not the one that was
/// discovered. The handshake must fail, and the request must never reach the
/// application layer of the impostor.
#[tokio::test]
async fn test_prepare_upload_rejected_on_fingerprint_mismatch() {
    let server_identity = generate_identity();
    let sender = generate_identity();
    let server = start_tls_server(&server_identity).await;

    // Pin some other device, e.g. the one the user actually picked.
    let expected = generate_identity();
    let client = client(&sender, Some(&expected.fingerprint));

    let files = vec![file_dto("file-1", 10)];
    let result = client
        .prepare_upload(
            ProtocolType::Https,
            "127.0.0.1",
            server.port,
            None,
            prepare_upload_request(&sender, &files),
            None,
            CancellationToken::new(),
        )
        .await;

    // A transport error, not a status code: the peer never got to answer.
    assert!(
        matches!(result, Err(ClientError::Reqwest(_))),
        "expected the handshake to fail, got {:?}",
        result.err()
    );

    // The decisive assertion: the request body never reached the peer.
    assert!(
        server.prepare_uploads.lock().await.is_empty(),
        "the impostor must not receive the file metadata"
    );
}

/// The file content must not be streamed to a peer that fails the pin check.
#[tokio::test]
async fn test_upload_body_not_sent_on_fingerprint_mismatch() {
    let server_identity = generate_identity();
    let sender = generate_identity();
    let server = start_tls_server(&server_identity).await;

    let expected = generate_identity();
    let client = client(&sender, Some(&expected.fingerprint));

    let content = b"secret".repeat(10_000);
    let (result, sent) = upload_bytes(
        &client,
        server.port,
        "some-session",
        "file-1",
        "some-token",
        &content,
    )
    .await;

    assert!(
        matches!(result, Err(ClientError::Reqwest(_))),
        "expected the handshake to fail, got {:?}",
        result.err()
    );
    assert_eq!(sent, 0, "no file content may be read or sent");
    assert!(server.received.lock().await.is_empty());
}

/// Without the web pages, the client certificate stays mandatory: a client
/// without one (e.g. a browser) must fail the handshake.
#[tokio::test]
async fn test_client_without_cert_rejected() {
    let server_identity = generate_identity();
    let server = start_tls_server(&server_identity).await;
    let client = LsHttpClientV2::try_new_without_cert().unwrap();

    let result = client
        .info(ProtocolType::Https, "127.0.0.1", server.port)
        .await;

    assert!(
        matches!(result, Err(ClientError::Reqwest(_))),
        "expected the handshake to fail, got {:?}",
        result.err()
    );
}

/// With the web pages served, the client certificate is optional so that
/// browsers can connect; a presented certificate is still verified (mTLS
/// clients keep working).
#[tokio::test]
async fn test_client_without_cert_allowed_in_web_mode() {
    let server_identity = generate_identity();
    let sender = generate_identity();
    let server = start_tls_server_with_web(
        &server_identity,
        WebConfig {
            mode: WebMode::Upload,
            ..WebConfig::default()
        },
    )
    .await;

    // A browser-like client: no client certificate, self-signed server cert accepted.
    let browser = localsend::reqwest::Client::builder()
        .use_rustls_tls()
        .danger_accept_invalid_certs(true)
        .build()
        .unwrap();
    let response = browser
        .get(format!("https://127.0.0.1:{}/", server.port))
        .send()
        .await
        .expect("browser without client certificate should be able to connect");
    assert_eq!(response.status().as_u16(), 200);
    assert!(response.text().await.unwrap().contains("prepare-upload"));

    // A LocalSend peer still authenticates with its certificate.
    let client = client(&sender, Some(&server_identity.fingerprint));
    let response = client
        .register(
            ProtocolType::Https,
            "127.0.0.1",
            server.port,
            sender_info(&sender.fingerprint),
        )
        .await
        .expect("register with client certificate should succeed");
    assert!(response.public_key.is_some());
}

/// Discovery has no fingerprint to pin yet, so any valid certificate is
/// accepted and the public key is read from the response.
#[tokio::test]
async fn test_register_without_pin_returns_public_key() {
    let server_identity = generate_identity();
    let sender = generate_identity();
    let server = start_tls_server(&server_identity).await;
    let client = client(&sender, None);

    let response = client
        .register(
            ProtocolType::Https,
            "127.0.0.1",
            server.port,
            sender_info(&sender.fingerprint),
        )
        .await
        .expect("register should succeed");

    assert!(response.public_key.is_some());
    assert_eq!(response.body.fingerprint, server_identity.fingerprint);
}

/// `info` reports the certificate its response came over, like `register`,
/// so a device that probes with it still identifies HTTPS peers by their
/// certificate.
#[tokio::test]
async fn test_info_reports_the_peer_certificate() {
    let server_identity = generate_identity();
    let sender = generate_identity();
    let server = start_tls_server(&server_identity).await;

    let info = client(&sender, None)
        .info(ProtocolType::Https, "127.0.0.1", server.port)
        .await
        .expect("info over TLS");

    assert_eq!(
        info.cert_fingerprint.as_deref(),
        Some(server_identity.fingerprint.as_str())
    );
    assert!(info.public_key.is_some());
    assert_eq!(info.body.alias, "Test Server");
}

/// Identities of either key type work with each other: a new ECDSA identity
/// with a peer that still generates RSA ones, and the other way round. Peers
/// trust each other by certificate fingerprint, not by key algorithm.
#[tokio::test]
async fn test_rsa_and_ecdsa_identities_interoperate() {
    let rsa = rsa_identity();
    let ecdsa = generate_identity();

    for (server_identity, sender) in [(&rsa, &ecdsa), (&ecdsa, &rsa)] {
        let server = start_tls_server(server_identity).await;
        let client = client(sender, Some(&server_identity.fingerprint));

        let info = client
            .info(ProtocolType::Https, "127.0.0.1", server.port)
            .await
            .expect("info over TLS");
        assert_eq!(
            info.cert_fingerprint.as_deref(),
            Some(server_identity.fingerprint.as_str())
        );

        // prepare-upload requires the client certificate (mTLS).
        let files = vec![file_dto("file-1", 5)];
        let result = client
            .prepare_upload(
                ProtocolType::Https,
                "127.0.0.1",
                server.port,
                None,
                prepare_upload_request(sender, &files),
                None,
                CancellationToken::new(),
            )
            .await
            .expect("prepare-upload should succeed");
        let response = result.response.expect("expected a session");
        let token = response.files.get("file-1").unwrap();
        let (upload_result, _) = upload_bytes(
            &client,
            server.port,
            &response.session_id,
            "file-1",
            token,
            b"hello",
        )
        .await;
        upload_result.expect("upload should succeed");
        assert_eq!(server.received.lock().await[0].1, b"hello");
    }
}
