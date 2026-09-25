//! Fleet node inventory — the single source the planet scene and the menu both
//! read. (Demo data baked from the morph reference; later fed from the live
//! mesh / inventory.)
//!
//! Every node is a neutral label only — country code plus an index within
//! that country (e.g. `"NL-1"`). No hoster-derived machine id, raw IP:port,
//! provider name, or operator infra-role string is ever carried here or
//! rendered by a consumer (`shell::menu`, `centerpiece::planet`) — see
//! mirage2app `CLAUDE.md` "No operator names in client UI".

/// One fleet node.
#[derive(Clone, Copy)]
pub struct FleetNode {
    /// Neutral, customer-safe label: country code + index within that
    /// country. Never a hoster-derived name, raw IP:port, or provider name.
    pub label: &'static str,
    pub lat: f64,
    pub lon: f64,
}

/// The 6 fleet nodes.
pub static FLEET: &[FleetNode] = &[
    FleetNode { label: "RU-1", lat: 55.75, lon: 37.62 },
    FleetNode { label: "RU-2", lat: 59.93, lon: 30.34 },
    FleetNode { label: "TR-1", lat: 41.01, lon: 28.98 },
    FleetNode { label: "NL-1", lat: 52.37, lon: 4.90 },
    FleetNode { label: "NL-2", lat: 51.92, lon: 4.48 },
    FleetNode { label: "CH-1", lat: 47.37, lon: 8.54 },
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

/// A mesh member — shown in the admin-only Admin tab. (Mock until the backend
/// returns the real roster; the family mesh has one admin + invited members.)
#[derive(Clone, Copy)]
pub struct Member {
    pub name: &'static str,
    pub role: &'static str,
}

/// The family mesh roster (admin pays for everything; members are invited).
pub static MEMBERS: &[Member] = &[
    Member { name: "admin@family", role: "admin" },
    Member { name: "alice",        role: "member" },
    Member { name: "bob",          role: "member" },
    Member { name: "carol",        role: "member" },
];

/// Catalog of nodes the admin can buy into the mesh.
pub static OFFERS: &[NodeOffer] = &[
    NodeOffer { region: "EU", label: "Amsterdam · NL", specs: "2 vCPU · 4 GB · 1 Gbps", prov: "HostKey",  price: "€7/mo" },
    NodeOffer { region: "EU", label: "Frankfurt · DE",  specs: "4 vCPU · 8 GB · 2 Gbps", prov: "HostKey",  price: "€13/mo" },
    NodeOffer { region: "RU", label: "Moscow · RU",     specs: "2 vCPU · 4 GB · 1 Gbps", prov: "HostKey",  price: "₽590/mo" },
    NodeOffer { region: "TR", label: "Istanbul · TR",   specs: "2 vCPU · 4 GB · 1 Gbps", prov: "HostKey",  price: "$9/mo" },
    NodeOffer { region: "US", label: "New York · US",   specs: "2 vCPU · 4 GB · 1 Gbps", prov: "Cloudzy",  price: "$11/mo" },
    NodeOffer { region: "CH", label: "Zurich · CH",     specs: "4 vCPU · 8 GB · 2 Gbps", prov: "Cloudzy",  price: "$18/mo" },
];
