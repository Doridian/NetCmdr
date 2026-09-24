// Copyright (C) 2026 Doridian
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Device operations: locking, backups, factory reset, certificates, firmware.
//!
//! Destructive commands require `--confirm`. The firmware upgrades are
//! **untested on hardware**.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use clap::Args;

use netcmdr_mgmt::{Client, ConfigDoc};

/// Refusal for the untested firmware commands. A failed flash can't be
/// recovered remotely, so they need a scarier flag than `--confirm`.
const BRICK_WARNING: &str = "\
this writes firmware, and it has never been run against hardware. The request \
format is transcribed from the vendor's client and nothing has confirmed the \
device accepts it, so it may well be wrong — and a firmware write that goes \
wrong, or is interrupted, leaves hardware that does not come back and cannot \
be recovered over the network. If you can reflash it out of band and you want \
to find out, pass --please-brick-my-device.";

fn read(path: &Path) -> Result<String> {
    std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))
}

fn write(path: &Path, contents: &str) -> Result<()> {
    std::fs::write(path, contents).with_context(|| format!("writing {}", path.display()))
}

fn refuse_to_overwrite(path: &Path, force: bool) -> Result<()> {
    if path.exists() && !force {
        bail!(
            "{} already exists; pass --force to overwrite it",
            path.display()
        );
    }
    Ok(())
}

// ---------------------------------------------------------------- export ---

pub fn export(client: &Client, path: Option<&PathBuf>, force: bool) -> Result<()> {
    let doc = client.config_doc().context("fetching the configuration")?;
    match path {
        Some(p) => {
            refuse_to_overwrite(p, force)?;
            write(p, &format!("{}\n", doc.to_xml()))?;
            eprintln!("configuration written to {}", p.display());
        }
        None => println!("{}", doc.to_xml()),
    }
    Ok(())
}

pub fn import(client: &Client, path: &Path, confirm: bool, set_clock: bool) -> Result<()> {
    if !confirm {
        bail!(
            "refusing to import without --confirm; this replaces every setting on the device \
             at once, and a network change will reboot it"
        );
    }
    let xml = read(path)?;
    let element = netcmdr_mgmt::xml::find_element(&xml, "config_data")
        .context("parsing the file")?
        .ok_or_else(|| anyhow::anyhow!("{} has no <config_data> element", path.display()))?;
    let mut doc = ConfigDoc::from_element(element).context("reading the file")?;
    if set_clock {
        doc.send_system_time();
    }

    let outcome = client
        .save_config(&doc)
        .context("saving the configuration")?;
    println!("configuration from {} applied", path.display());
    if outcome.rebooting {
        println!("the device is rebooting to apply this");
    }
    Ok(())
}

// ---------------------------------------------------------------- backup ---

pub fn backup(client: &Client, path: &Path, force: bool) -> Result<()> {
    refuse_to_overwrite(path, force)?;
    let data = client.backup().context("taking a backup")?;
    write(path, &data)?;
    println!(
        "backup written to {} ({} bytes)",
        path.display(),
        data.len()
    );
    Ok(())
}

#[derive(Args)]
pub struct RestoreArgs {
    /// A file written by `netcmdr backup`.
    pub file: PathBuf,

    /// Keep this device's network settings instead of the backup's.
    #[arg(long)]
    pub keep_network: bool,

    /// Required: restoring replaces every setting and reboots the device.
    #[arg(long)]
    pub confirm: bool,
}

pub fn restore(client: &Client, a: &RestoreArgs) -> Result<()> {
    if !a.confirm {
        bail!("refusing to restore without --confirm; this replaces every setting and reboots");
    }
    let data = read(&a.file)?;
    if data.trim().is_empty() {
        bail!("{} is empty", a.file.display());
    }
    client
        .restore(data.trim(), a.keep_network)
        .context("restoring the backup")?;
    println!(
        "backup restored{}; the device is rebooting",
        if a.keep_network {
            ", network settings kept"
        } else {
            ""
        }
    );
    Ok(())
}

#[derive(Args)]
pub struct FactoryResetArgs {
    /// Keep the network settings, so the device comes back reachable.
    #[arg(long)]
    pub keep_network: bool,

    /// Required: this erases every setting, including the accounts.
    #[arg(long)]
    pub confirm: bool,
}

pub fn factory_reset(client: &Client, a: &FactoryResetArgs) -> Result<()> {
    if !a.confirm {
        bail!(
            "refusing to reset without --confirm; this erases every setting and every account, \
             and puts the login back to the factory admin/access"
        );
    }
    client
        .factory_restore(a.keep_network)
        .context("restoring factory defaults")?;
    println!(
        "factory defaults restored{}; the device is rebooting",
        if a.keep_network {
            ", network settings kept"
        } else {
            ""
        }
    );
    if !a.keep_network {
        println!("it will come back on DHCP, as admin/access");
    }
    Ok(())
}

// -------------------------------------------------------------- sessions ---

pub fn lock(client: &Client) -> Result<()> {
    client.lock_system().context("locking the device")?;
    println!("every other user is locked out until `netcmdr unlock`");
    Ok(())
}

pub fn unlock(client: &Client) -> Result<()> {
    client.unlock_system().context("unlocking the device")?;
    println!("other users may connect again");
    Ok(())
}

pub fn disconnect(client: &Client, port: u32) -> Result<()> {
    client
        .disconnect_port(port)
        .with_context(|| format!("disconnecting port {port}"))?;
    println!("whoever was on port {port} has been disconnected");
    Ok(())
}

/// Release a stale configuration lock.
pub fn release_lock(client: &Client) -> Result<()> {
    client
        .permission(netcmdr_mgmt::Permission::Normal)
        .context("releasing the configuration lock")?;
    println!("configuration lock released");
    Ok(())
}

// ----------------------------------------------------------- certificate ---

#[derive(Args)]
pub struct CertificateArgs {
    /// PEM certificate to serve on the management port.
    #[arg(long, value_name = "FILE")]
    pub certificate: PathBuf,

    /// PEM private key for that certificate.
    #[arg(long, value_name = "FILE")]
    pub key: PathBuf,

    /// Passphrase on the private key, if it has one.
    #[arg(long, value_name = "PASSWORD", default_value = "")]
    pub key_password: String,

    /// Required: the device restarts its web server, dropping sessions.
    #[arg(long)]
    pub confirm: bool,
}

pub fn install_certificate(client: &Client, a: &CertificateArgs) -> Result<()> {
    if !a.confirm {
        bail!(
            "refusing to install without --confirm; the device restarts its management server, \
             and a certificate it cannot load leaves it unreachable over HTTPS"
        );
    }
    let cert = read(&a.certificate)?;
    let key = read(&a.key)?;
    if !cert.contains("-----BEGIN") {
        bail!(
            "{} does not look like a PEM certificate",
            a.certificate.display()
        );
    }
    if !key.contains("-----BEGIN") {
        bail!("{} does not look like a PEM private key", a.key.display());
    }

    client
        .install_certificate(&cert, &key, &a.key_password)
        .context("installing the certificate")?;
    println!("certificate installed; the device is restarting its management server");
    Ok(())
}

// --------------------------------------------------------------- upgrade ---

#[derive(Args)]
pub struct UpgradeArgs {
    /// The firmware file from the vendor.
    pub file: PathBuf,

    /// Required. Untested on hardware; a failed flash bricks the device.
    #[arg(long)]
    pub please_brick_my_device: bool,
}

pub fn upgrade(client: &Client, a: &UpgradeArgs) -> Result<()> {
    if !a.please_brick_my_device {
        bail!("{}", BRICK_WARNING);
    }
    let image = read(&a.file)?;
    client
        .upgrade_device(&image)
        .context("uploading the firmware")?;
    println!("firmware uploaded; watch it with `netcmdr upgrade-status`");
    Ok(())
}

pub fn upgrade_status(client: &Client) -> Result<()> {
    let s = client.upgrade_status().context("reading upgrade status")?;
    match s.percent_complete {
        Some(pct) => println!("{} ({pct}%)", s.status),
        None => println!("{}", s.status),
    }
    Ok(())
}

#[derive(Args)]
pub struct SiuUpgradeArgs {
    /// The SIU firmware file from the vendor.
    pub file: PathBuf,

    /// Ports whose SIUs to upgrade: `all`, or e.g. `1,3-5`.
    #[arg(long, value_name = "PORTS", default_value = "all")]
    pub ports: String,

    /// Required. Untested on hardware; a failed flash bricks the SIU.
    #[arg(long)]
    pub please_brick_my_device: bool,
}

pub fn siu_upgrade(client: &Client, a: &SiuUpgradeArgs, port_count: u32) -> Result<()> {
    if !a.please_brick_my_device {
        bail!("{}", BRICK_WARNING);
    }
    let ports = crate::args::parse_ports(&a.ports, port_count)?;
    if ports.is_empty() {
        bail!("no ports selected");
    }
    let image = read(&a.file)?;
    client
        .upgrade_sius(&image, &ports)
        .context("uploading the SIU firmware")?;
    println!(
        "SIU firmware uploaded for {} port(s); watch it with `netcmdr siu upgrade-status`",
        ports.len()
    );
    Ok(())
}

pub fn siu_upgrade_status(client: &Client) -> Result<()> {
    let all = client
        .siu_upgrade_status()
        .context("reading SIU upgrade status")?;
    if all.is_empty() {
        println!("no SIU upgrade in progress");
        return Ok(());
    }
    for s in all {
        match s.percent_complete {
            Some(pct) => println!("{:>2}  {} ({pct}%)", s.port, s.status),
            None => println!("{:>2}  {}", s.port, s.status),
        }
    }
    Ok(())
}

pub fn siu_versions(client: &Client, spec: &str, port_count: u32) -> Result<()> {
    let ports = crate::args::parse_ports(spec, port_count)?;
    if ports.is_empty() {
        bail!("no ports selected");
    }

    // One slow round trip per port, so print as we go.
    println!("{:>4}  {:<32} FIRMWARE", "PORT", "HARDWARE");
    let mut answered = 0;
    for port in &ports {
        let v = client
            .siu_version(*port)
            .with_context(|| format!("reading the SIU version on port {port}"))?;
        if v.responding() {
            answered += 1;
            println!(
                "{:>4}  {:<32} {}",
                v.port,
                crate::args::or_dash(v.hardware.as_deref()),
                crate::args::or_dash(v.firmware.as_deref()),
            );
        } else {
            println!("{:>4}  no SIU attached", v.port);
        }
    }
    if answered == 0 {
        println!("\nno SIU answered on any of the ports asked");
    }
    Ok(())
}
