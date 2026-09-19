//! `b070-capture` — a TLS-intercepting proxy for the KVM session protocol.
//!
//! Point the stock Java client at this instead of the KVM. It terminates the
//! client's TLS with a generated certificate, opens its own TLS session to the
//! real device, and writes every byte of both directions to disk while
//! attempting to decode `KVMMsg` frames live.
//!
//! This works because the vendor client does not verify certificates
//! (`com.minicom.plugin.socket.SocketUtils` installs a trust-all
//! `X509TrustManager`).
//!
//! **Known limitation — needs rework for current firmware.** Live analysis of a
//! device (firmware 2.1.1184) showed the session is not TLS-on-connect. Each of
//! the four channels (`SocketType` cmd/kbmouse/video/virtualmedia) opens with a
//! *plaintext* handshake — the client writes `"<sessionId>#"` then a 4-byte
//! socket-type code and reads it back — and only the cmd and keyboard/mouse
//! channels then upgrade to TLS; video and virtual media stay in the clear. So
//! this proxy, which TLS-terminates immediately, matches none of them as-is. It
//! is kept for the framing decoder and as the skeleton for that rework; see
//! `research/decompiled/app-live/com/minicom/plugin/socket/KVMSocket.java`.
//!
//! ```text
//! b070-capture --device 10.0.0.5 --listen 0.0.0.0:900 --out ./capture
//! ```
//!
//! Output per connection:
//! - `<n>-c2s.bin` / `<n>-s2c.bin` — raw plaintext streams
//! - `<n>-frames.log`              — decoded frame log
//!
//! Intended for use against a device you administer. Both directions are
//! written in the clear, including the credentials in the `connect` command.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use anyhow::{Context, Result};
use b070_proto::kvmmsg;
use clap::Parser;
use tokio::fs::File;
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufWriter};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use tokio_rustls::rustls::{ClientConfig, ServerConfig};
use tokio_rustls::{TlsAcceptor, TlsConnector};

mod noverify;

#[derive(Parser)]
#[command(
    name = "b070-capture",
    about = "TLS-intercepting capture proxy for the NetCommander IP KVM session protocol",
    version
)]
struct Cli {
    /// Real device hostname or IP.
    #[arg(long)]
    device: String,

    /// Device session port.
    #[arg(long, default_value_t = b070_proto::DEFAULT_SESSION_PORT)]
    device_port: u16,

    /// Address to listen on for the client.
    #[arg(long, default_value = "0.0.0.0:900")]
    listen: SocketAddr,

    /// Directory for capture files.
    #[arg(long, default_value = "./capture")]
    out: PathBuf,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "b070_capture=info".into()),
        )
        .init();

    let cli = Cli::parse();
    tokio::fs::create_dir_all(&cli.out)
        .await
        .with_context(|| format!("creating {}", cli.out.display()))?;

    // rustls needs a process-wide crypto provider before any config is built.
    let _ = tokio_rustls::rustls::crypto::ring::default_provider().install_default();

    let acceptor = build_acceptor().context("building TLS server config")?;
    let connector = build_connector().context("building TLS client config")?;

    let listener = TcpListener::bind(cli.listen)
        .await
        .with_context(|| format!("binding {}", cli.listen))?;
    tracing::info!(
        listen = %cli.listen,
        device = %format!("{}:{}", cli.device, cli.device_port),
        out = %cli.out.display(),
        "capture proxy ready"
    );
    tracing::warn!("captures contain credentials in cleartext");

    let counter = Arc::new(AtomicU64::new(0));

    loop {
        let (client, peer) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                tracing::error!(error = %e, "accept failed");
                continue;
            }
        };
        let n = counter.fetch_add(1, Ordering::Relaxed);
        let acceptor = acceptor.clone();
        let connector = connector.clone();
        let device = cli.device.clone();
        let device_port = cli.device_port;
        let out = cli.out.clone();

        tokio::spawn(async move {
            tracing::info!(conn = n, %peer, "connection opened");
            if let Err(e) =
                handle(n, client, acceptor, connector, &device, device_port, &out).await
            {
                tracing::error!(conn = n, error = %format!("{e:#}"), "connection failed");
            } else {
                tracing::info!(conn = n, "connection closed");
            }
        });
    }
}

/// Generate a throwaway server certificate.
///
/// The subject matches the vendor's own shipped cert (`*.kvm.net`) purely for
/// cosmetic similarity — the client checks neither the name nor the chain.
fn build_acceptor() -> Result<TlsAcceptor> {
    let cert = rcgen::generate_simple_self_signed(vec![
        "*.kvm.net".to_string(),
        "localhost".to_string(),
    ])?;
    let cert_der = CertificateDer::from(cert.cert.der().to_vec());
    let key_der = PrivateKeyDer::try_from(cert.key_pair.serialize_der())
        .map_err(|e| anyhow::anyhow!("serializing private key: {e}"))?;

    let cfg = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert_der], key_der)?;
    Ok(TlsAcceptor::from(Arc::new(cfg)))
}

/// Client config that accepts whatever the device presents.
fn build_connector() -> Result<TlsConnector> {
    let cfg = ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(noverify::NoVerify::new()))
        .with_no_client_auth();
    Ok(TlsConnector::from(Arc::new(cfg)))
}

async fn handle(
    n: u64,
    client: TcpStream,
    acceptor: TlsAcceptor,
    connector: TlsConnector,
    device: &str,
    device_port: u16,
    out: &Path,
) -> Result<()> {
    client.set_nodelay(true).ok();
    let client = acceptor
        .accept(client)
        .await
        .context("TLS handshake with client")?;

    let upstream = TcpStream::connect((device, device_port))
        .await
        .with_context(|| format!("connecting to {device}:{device_port}"))?;
    upstream.set_nodelay(true).ok();
    let server_name = ServerName::try_from(device.to_string())
        .unwrap_or_else(|_| ServerName::try_from("kvm.invalid").unwrap());
    let upstream = connector
        .connect(server_name, upstream)
        .await
        .context("TLS handshake with device")?;

    let (mut cr, mut cw) = tokio::io::split(client);
    let (mut sr, mut sw) = tokio::io::split(upstream);

    let c2s_path = out.join(format!("{n:04}-c2s.bin"));
    let s2c_path = out.join(format!("{n:04}-s2c.bin"));
    let log_path = out.join(format!("{n:04}-frames.log"));

    let mut c2s = BufWriter::new(File::create(&c2s_path).await?);
    let mut s2c = BufWriter::new(File::create(&s2c_path).await?);
    let log = Arc::new(tokio::sync::Mutex::new(BufWriter::new(
        File::create(&log_path).await?,
    )));

    let log_a = log.clone();
    let a = async move {
        pump(&mut cr, &mut sw, &mut c2s, "C->S", log_a).await;
    };
    let log_b = log.clone();
    let b = async move {
        pump(&mut sr, &mut cw, &mut s2c, "S->C", log_b).await;
    };
    tokio::join!(a, b);

    log.lock().await.flush().await.ok();
    tracing::info!(conn = n, frames = %log_path.display(), "capture written");
    Ok(())
}

/// Copy one direction, tee to disk, and decode frames as they go.
async fn pump<R, W>(
    from: &mut R,
    to: &mut W,
    tee: &mut BufWriter<File>,
    label: &str,
    log: Arc<tokio::sync::Mutex<BufWriter<File>>>,
) where
    R: AsyncReadExt + Unpin,
    W: AsyncWriteExt + Unpin,
{
    let mut buf = vec![0u8; 64 * 1024];
    let mut pending: Vec<u8> = Vec::new();
    let mut offset: u64 = 0;

    loop {
        let n = match from.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        let chunk = &buf[..n];
        if to.write_all(chunk).await.is_err() {
            break;
        }
        let _ = to.flush().await;
        let _ = tee.write_all(chunk).await;

        pending.extend_from_slice(chunk);
        let consumed = decode_frames(&pending, label, offset, &log).await;
        if consumed > 0 {
            pending.drain(..consumed);
            offset += consumed as u64;
        }
        // Don't let an undecodable stream grow without bound. Once we are this
        // far out of sync, framing is lost for this direction anyway; keep a
        // window so a later resync is still possible.
        const MAX_PENDING: usize = 8 * 1024 * 1024;
        if pending.len() > MAX_PENDING {
            let drop = pending.len() - MAX_PENDING;
            pending.drain(..drop);
            offset += drop as u64;
        }
    }
    let _ = tee.flush().await;
    let _ = to.shutdown().await;
}

/// Decode as many whole `KVMMsg` frames as `buf` holds; return bytes consumed.
async fn decode_frames(
    buf: &[u8],
    label: &str,
    base_offset: u64,
    log: &Arc<tokio::sync::Mutex<BufWriter<File>>>,
) -> usize {
    let mut pos = 0usize;
    let mut lines = String::new();

    while pos < buf.len() {
        match kvmmsg::Message::parse(&buf[pos..]) {
            Ok((msg, used)) => {
                let h = msg.header;
                lines.push_str(&format!(
                    "{label} @{:#010x}  magic={:?} flag={:?} cmd={:?} len={} id={} \
                     kf={} sts={:#010x} cts={:#010x}  {}\n",
                    base_offset + pos as u64,
                    h.magic,
                    h.cmd_flag,
                    h.command,
                    h.size,
                    h.id,
                    h.key_frame,
                    h.server_timestamp,
                    h.client_timestamp,
                    hex_preview(&msg.payload, 48),
                ));
                pos += used;
            }
            // Not enough bytes yet — wait for more.
            Err(b070_proto::Error::Truncated { .. }) => break,
        }
    }

    if !lines.is_empty() {
        let mut g = log.lock().await;
        let _ = g.write_all(lines.as_bytes()).await;
        let _ = g.flush().await;
    }
    pos
}

fn hex_preview(b: &[u8], max: usize) -> String {
    let n = b.len().min(max);
    let mut s = String::with_capacity(n * 3 + 8);
    for byte in &b[..n] {
        s.push_str(&format!("{byte:02x}"));
    }
    if b.len() > n {
        s.push('…');
    }
    s
}
