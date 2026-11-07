//! `EndpointProtocol` — переехал в `mirage-protocol::endpoint_protocol`.
//!
//! Сохранён как re-export для backward compat: cli / sentinel / tun
//! продолжают писать `mirage_client_core::endpoint_protocol::EndpointProtocol`.

pub use mirage_protocol::endpoint_protocol::EndpointProtocol;
