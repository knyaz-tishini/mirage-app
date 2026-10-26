use super::config::TunConfig;
use super::traits::{PlatformFirewall, PlatformRouting, PlatformTun};

pub struct TunStack {
    tun: Box<dyn PlatformTun>,
    firewall: Box<dyn PlatformFirewall>,
    routing: Box<dyn PlatformRouting>,
    config: TunConfig,
}

impl TunStack {
    pub fn new(
        tun: Box<dyn PlatformTun>,
        firewall: Box<dyn PlatformFirewall>,
        routing: Box<dyn PlatformRouting>,
        config: TunConfig,
    ) -> Self {
        Self {
            tun,
            firewall,
            routing,
            config,
        }
    }
}
