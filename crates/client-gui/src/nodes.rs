//! Fleet node inventory — the single source the planet scene and the menu both
//! read. (Demo data baked from the morph reference; later fed from the live
//! mesh / inventory.)

/// One fleet node.
#[derive(Clone, Copy)]
pub struct FleetNode {
    pub reg: &'static str,
    pub city: &'static str,
    pub ip: &'static str,
    pub id: &'static str,
    pub prov: &'static str,
    pub roles: &'static str,
    pub lat: f64,
    pub lon: f64,
}

const ROLES: &str = "monitor · node · sentry · mshd · db-worker";
const ROLES_SA: &str = "monitor · node · sentry · mshd · db-worker · standalone";

/// The 6 fleet nodes.
pub static FLEET: &[FleetNode] = &[
    FleetNode { reg: "RU", city: "Moscow", ip: "193.187.94.74", id: "ru-hostkey-094.74", prov: "HostKey", roles: ROLES, lat: 55.75, lon: 37.62 },
    FleetNode { reg: "RU", city: "St Petersburg", ip: "91.218.245.247", id: "ru-hostkey-245.247", prov: "HostKey", roles: ROLES, lat: 59.93, lon: 30.34 },
    FleetNode { reg: "TR", city: "Istanbul", ip: "82.26.94.247", id: "tr-hostkey-094.247", prov: "HostKey", roles: ROLES, lat: 41.01, lon: 28.98 },
    FleetNode { reg: "NL", city: "Amsterdam", ip: "45.12.212.226", id: "nl-hosthatch-212.226", prov: "Omro", roles: ROLES_SA, lat: 52.37, lon: 4.90 },
    FleetNode { reg: "NL", city: "Rotterdam", ip: "151.243.173.104", id: "nl-hostkey-173.104", prov: "HostKey", roles: ROLES, lat: 51.92, lon: 4.48 },
    FleetNode { reg: "CH", city: "Zurich", ip: "45.59.119.139", id: "ch-cloudzy-119.139", prov: "Cloudzy", roles: ROLES_SA, lat: 47.37, lon: 8.54 },
];

/// Links between fleet nodes (indices) — used by the planet mesh.
pub static LINKS: &[(usize, usize)] = &[(0, 2), (2, 5), (1, 3), (3, 5), (0, 1), (4, 2)];

/// A purchasable node offering — the "buy a node into the mesh" catalog.
/// (Demo pricing; later fed from hoster APIs → issue → pay → auto-join mesh.)
#[derive(Clone, Copy)]
pub struct NodeOffer {
    pub region: &'static str,
    pub label: &'static str,
    pub specs: &'static str,
    pub prov: &'static str,
    pub price: &'static str,
}

/// Catalog of nodes the admin can buy into the mesh.
pub static OFFERS: &[NodeOffer] = &[
    NodeOffer { region: "EU", label: "Amsterdam · NL", specs: "2 vCPU · 4 GB · 1 Gbps", prov: "HostKey",  price: "€7/mo" },
    NodeOffer { region: "EU", label: "Frankfurt · DE",  specs: "4 vCPU · 8 GB · 2 Gbps", prov: "HostKey",  price: "€13/mo" },
    NodeOffer { region: "RU", label: "Moscow · RU",     specs: "2 vCPU · 4 GB · 1 Gbps", prov: "HostKey",  price: "₽590/mo" },
    NodeOffer { region: "TR", label: "Istanbul · TR",   specs: "2 vCPU · 4 GB · 1 Gbps", prov: "HostKey",  price: "$9/mo" },
    NodeOffer { region: "US", label: "New York · US",   specs: "2 vCPU · 4 GB · 1 Gbps", prov: "Cloudzy",  price: "$11/mo" },
    NodeOffer { region: "CH", label: "Zurich · CH",     specs: "4 vCPU · 8 GB · 2 Gbps", prov: "Cloudzy",  price: "$18/mo" },
];
