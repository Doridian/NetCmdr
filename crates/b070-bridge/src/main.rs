// Copyright (C) 2026 Doridian
// SPDX-License-Identifier: AGPL-3.0-or-later
//! `b070-bridge` — the binary. Everything it does lives in the library beside
//! it; this is argument parsing and the accept loop.

use std::net::TcpListener;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use b070_bridge::assets::Assets;
use b070_bridge::{serve, Bridge};
use clap::Parser;

#[derive(Parser, Debug)]
#[command(
    name = "b070-bridge",
    about = "Serve the web client for a NetCommander IP KVM and relay its protocol",
    long_about = "Terminates the device's legacy TLS — which no browser can negotiate — and \
                  offers the same protocol over plain HTTP and WebSockets, so a browser can \
                  run the KVM client.\n\n\
                  The bridge authenticates nobody: reaching it means reaching the device. \
                  Keep the default loopback listen address unless something in front of it \
                  is doing the authenticating."
)]
struct Args {
    /// Device address.
    #[arg(long, env = "B070_HOST")]
    host: String,

    /// Device management (HTTPS) port.
    #[arg(long, default_value_t = b070_proto::DEFAULT_HTTPS_PORT)]
    https_port: u16,

    /// Device session port. The login reply advertises this; override it if the
    /// device has been moved off the default.
    #[arg(long, default_value_t = b070_proto::DEFAULT_SESSION_PORT)]
    session_port: u16,

    /// Address to listen on.
    #[arg(long, default_value = "127.0.0.1:8080")]
    listen: String,

    /// Directory holding the web client to serve.
    #[arg(long, value_name = "DIR")]
    static_dir: Option<PathBuf>,

    /// How long to wait on the device.
    #[arg(long, default_value = "20", value_name = "SECONDS")]
    timeout: u64,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "b070_bridge=info,b070_session=info".into()),
        )
        .init();

    let args = Args::parse();
    // Compressing starts now and finishes somewhere into the first minute; the
    // listener below does not wait for it.
    let assets = args.static_dir.map(|dir| {
        let assets = Assets::new(dir);
        assets.warm_in_background();
        assets
    });
    let bridge = Bridge {
        host: args.host,
        https_port: args.https_port,
        session_port: args.session_port,
        assets,
        timeout: Duration::from_secs(args.timeout),
    };

    let listener =
        TcpListener::bind(&args.listen).with_context(|| format!("listening on {}", args.listen))?;
    let bound = listener.local_addr()?;

    if bound.ip().is_loopback() {
        tracing::info!(%bound, device = %bridge.host, "bridge ready");
    } else {
        tracing::warn!(
            %bound, device = %bridge.host,
            "listening off-loopback: anything that can reach this can reach the device"
        );
    }
    if bridge.assets.is_none() {
        tracing::info!("no --static-dir: serving the protocol routes only");
    }

    for incoming in listener.incoming() {
        let stream = match incoming {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, "accept failed");
                continue;
            }
        };
        let bridge = bridge.clone();
        // One thread per connection. A session is four sockets and a handful of
        // management calls, so the count stays in the low tens.
        if let Err(e) = std::thread::Builder::new()
            .name("b070-bridge-conn".into())
            .spawn(move || serve(&bridge, stream))
        {
            tracing::warn!(error = %e, "could not spawn a connection thread");
        }
    }
    Ok(())
}
