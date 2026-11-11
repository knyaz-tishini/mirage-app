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

/// The backend the window is asked for, out of the command line.
///
/// `--backend <name>` or `--backend=<name>`.  Absent, the runtime
/// autodetects: the adapter of the machine decides.
pub fn from_args() -> Option<RenderBackend> {
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if let Some(name) = arg.strip_prefix("--backend=") {
            return parse_backend(name);
        }
        if arg == "--backend" {
            return args.next().as_deref().and_then(parse_backend);
        }
    }
    None
}
