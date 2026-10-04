//! `--backend <name>` — pick the initial render backend by name.
//!
//! The customer-facing use is the support fallback for machines with a broken
//! GPU driver (`--backend tiny_skia`); absent, the runtime autodetects.

use uzor::platform::types::RenderBackend;

/// Map a backend name to a [`RenderBackend`].
///
/// `name` is one of `vello_gpu | vello_hybrid | vello_cpu | instanced_wgpu |
/// urx_wgpu | urx_wgpu_full | urx_hybrid | urx_cpu | tiny_skia`.
pub fn parse_backend(name: &str) -> Option<RenderBackend> {
    match name.trim() {
        "vello_gpu" => Some(RenderBackend::VelloGpu),
        "vello_hybrid" => Some(RenderBackend::VelloHybrid),
        "vello_cpu" => Some(RenderBackend::VelloCpu),
        "instanced_wgpu" => Some(RenderBackend::InstancedWgpu),
        "urx_wgpu" => Some(RenderBackend::UrxWgpu),
        "urx_wgpu_full" => Some(RenderBackend::UrxWgpuFull),
        "urx_hybrid" => Some(RenderBackend::UrxHybrid),
        "urx_cpu" => Some(RenderBackend::UrxCpu),
        "tiny_skia" => Some(RenderBackend::TinySkia),
        _ => None,
    }
}
