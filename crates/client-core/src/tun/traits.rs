use std::net::{IpAddr, SocketAddr};

use async_trait::async_trait;
use mirage_core::error::MirageError;

#[async_trait]
pub trait PlatformTun: Send {
    async fn create(&mut self) -> Result<(), MirageError>;
    async fn read_packet(&mut self, buf: &mut [u8]) -> Result<usize, MirageError>;
    async fn write_packet(&mut self, packet: &[u8]) -> Result<(), MirageError>;
    fn name(&self) -> &str;
    fn destroy(&mut self);
}

pub trait PlatformFirewall: Send {
    fn activate(
        &mut self,
        relay_addrs: &[SocketAddr],
        tun_name: &str,
    ) -> Result<(), MirageError>;
    fn deactivate(&mut self) -> Result<(), MirageError>;
}

pub trait PlatformRouting: Send {
    fn set_default_route(&mut self, tun_name: &str) -> Result<(), MirageError>;
    fn restore_routes(&mut self) -> Result<(), MirageError>;
    fn add_bypass(&mut self, addr: IpAddr) -> Result<(), MirageError>;
}
