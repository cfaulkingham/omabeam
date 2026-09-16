//! Local-only QR generation. No share token is passed to a subprocess or service.
use anyhow::{Context, Result, ensure};
use qrcode::{Color, EcLevel, QrCode};
use serde::Serialize;

/// Compact, bounded module rows for the bar's QtQuick canvas (no image decoder).
#[derive(Serialize)]
pub struct ShareQr {
    pub url: String,
    pub rows: Vec<String>,
}

impl ShareQr {
    pub fn new(url: &str) -> Result<Self> {
        crate::localsend::ensure_share_url(url)?;
        let code = QrCode::with_error_correction_level(url.as_bytes(), EcLevel::M)
            .context("Could not generate the share QR code")?;
        ensure!(code.width() <= 81, "Share QR code exceeds the size limit");
        let rows = (0..code.width())
            .map(|y| {
                (0..code.width())
                    .map(|x| {
                        if code[(x, y)] == Color::Dark {
                            '1'
                        } else {
                            '0'
                        }
                    })
                    .collect()
            })
            .collect();
        Ok(Self {
            url: url.into(),
            rows,
        })
    }
}

pub fn current_share() -> Result<ShareQr> {
    let status = crate::live::latest_status_report()?
        .context("No live share is running. Start sharing first.")?;
    ensure!(status.stats.state == "live", "The share has ended.");
    ShareQr::new(&status.url)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_qr_for_ipv4_and_ipv6_links() {
        for host in ["192.168.1.24", "[fd00:1234:5678:abcd:1234:5678:abcd:1234]"] {
            let url = format!("http://{host}:65535/s/0123456789abcdef0123456789abcdef/");
            let qr = ShareQr::new(&url).unwrap();
            assert_eq!(qr.url, url);
            assert!(qr.rows.len() >= 21 && qr.rows.len() <= 81);
            assert!(qr.rows.iter().all(
                |row| row.len() == qr.rows.len() && row.bytes().all(|b| b == b'0' || b == b'1')
            ));
            assert_eq!(&qr.rows[0][..7], "1111111");
            assert!(serde_json::to_vec(&qr).unwrap().len() < 8192);
            // Independently decode the same black/white grid and quiet zone the bar draws.
            let scale = 4;
            let side = (qr.rows.len() + 8) * scale;
            let mut pixels = vec![255; side * side];
            for (y, row) in qr.rows.iter().enumerate() {
                for (x, bit) in row.bytes().enumerate() {
                    if bit == b'1' {
                        for dy in 0..scale {
                            for dx in 0..scale {
                                pixels[((y + 4) * scale + dy) * side + (x + 4) * scale + dx] = 0;
                            }
                        }
                    }
                }
            }
            let mut decoder = quircs::Quirc::default();
            let codes: Vec<_> = decoder.identify(side, side, &pixels).collect();
            assert_eq!(codes.len(), 1);
            assert_eq!(
                codes[0].as_ref().unwrap().decode().unwrap().payload,
                url.as_bytes()
            );
        }
    }

    #[test]
    fn rejects_non_share_links_without_echoing_them() {
        for url in [
            "file:///etc/passwd",
            "https://example.com/secret",
            "http://127.0.0.1:9847/s/bad/",
            &"x".repeat(257),
        ] {
            let error = ShareQr::new(url).err().unwrap().to_string();
            assert!(!error.contains(url));
        }
    }
}
