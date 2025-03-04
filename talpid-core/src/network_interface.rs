use nix::fcntl;
use std::{
    io,
    net::IpAddr,
    os::unix::io::{AsRawFd, IntoRawFd, RawFd},
};
use tun::{platform, Configuration, Device};

/// Errors that can happen when working with *nix tunnel interfaces.
#[derive(err_derive::Error, Debug)]
#[error(no_from)]
pub enum Error {
    /// Failed to set IP address
    #[error(display = "Failed to set IPv4 address")]
    SetIpv4Error(#[error(source)] tun::Error),

    /// Failed to set IP address
    #[error(display = "Failed to set IPv6 address")]
    SetIpv6Error(#[error(source)] io::Error),

    /// Unable to open a tunnel device
    #[error(display = "Unable to open a tunnel device")]
    CreateDeviceError(#[error(source)] tun::Error),

    /// Failed to apply async flags to tunnel device
    #[error(display = "Failed to apply async flags to tunnel device")]
    SetDeviceAsyncError(#[error(source)] nix::Error),

    /// Failed to enable/disable link device
    #[error(display = "Failed to enable/disable link device")]
    ToggleDeviceError(#[error(source)] tun::Error),
}

/// A trait for managing link devices
pub trait NetworkInterface: Sized {
    /// Bring a given interface up or down
    fn set_up(&mut self, up: bool) -> Result<(), Error>;

    /// Set host IPs for interface
    fn set_ip(&mut self, ip: IpAddr) -> Result<(), Error>;

    /// Set MTU for interface
    fn set_mtu(&mut self, mtu: u16) -> Result<(), Error>;

    /// Get name of interface
    fn get_name(&self) -> &str;
}

trait WireguardLink: AsRawFd + IntoRawFd {}

fn apply_async_flags(fd: RawFd) -> Result<(), nix::Error> {
    fcntl::fcntl(fd, fcntl::FcntlArg::F_GETFL)?;
    let arg = fcntl::FcntlArg::F_SETFL(fcntl::OFlag::O_RDWR | fcntl::OFlag::O_NONBLOCK);
    fcntl::fcntl(fd, arg)?;
    Ok(())
}

/// A tunnel devie
pub struct TunnelDevice {
    dev: platform::Device,
}

impl TunnelDevice {
    /// Creates a new Tunnel device
    #[allow(unused_mut)]
    pub fn new() -> Result<Self, Error> {
        let mut config = Configuration::default();

        #[cfg(target_os = "linux")]
        config.platform(|config| {
            config.packet_information(true);
        });
        let mut dev = platform::create(&config).map_err(Error::CreateDeviceError)?;
        apply_async_flags(dev.as_raw_fd()).map_err(Error::SetDeviceAsyncError)?;
        Ok(Self { dev })
    }
}

impl AsRawFd for TunnelDevice {
    fn as_raw_fd(&self) -> RawFd {
        self.dev.as_raw_fd()
    }
}

impl IntoRawFd for TunnelDevice {
    fn into_raw_fd(self) -> RawFd {
        self.dev.into_raw_fd()
    }
}

impl NetworkInterface for TunnelDevice {
    fn set_ip(&mut self, ip: IpAddr) -> Result<(), Error> {
        match ip {
            IpAddr::V4(ipv4) => self.dev.set_address(ipv4).map_err(Error::SetIpv4Error),
            IpAddr::V6(ipv6) => {
                #[cfg(target_os = "linux")]
                {
                    duct::cmd!(
                        "ip",
                        "-6",
                        "addr",
                        "add",
                        ipv6.to_string(),
                        "dev",
                        self.dev.name()
                    )
                    .run()
                    .map(|_| ())
                    .map_err(Error::SetIpv6Error)
                }
                #[cfg(target_os = "macos")]
                {
                    duct::cmd!(
                        "ifconfig",
                        self.dev.name(),
                        "inet6",
                        ipv6.to_string(),
                        "alias"
                    )
                    .run()
                    .map(|_| ())
                    .map_err(Error::SetIpv6Error)
                }
            }
        }
    }

    fn set_up(&mut self, up: bool) -> Result<(), Error> {
        self.dev.enabled(up).map_err(Error::ToggleDeviceError)
    }

    fn set_mtu(&mut self, mtu: u16) -> Result<(), Error> {
        self.dev
            .set_mtu(i32::from(mtu))
            .map_err(Error::ToggleDeviceError)
    }

    fn get_name(&self) -> &str {
        self.dev.name()
    }
}
