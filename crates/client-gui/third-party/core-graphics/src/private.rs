// The published crate called private SkyLight entry points from this module.
// Nothing in this app uses the types. The bodies stay so the public API still
// compiles, and they do not reference those entry points.

use crate::geometry::CGRect;
use libc::{c_int, c_uint};
use std::ptr;

enum RegionObject {}
type RegionRef = *mut RegionObject;

pub struct CGSRegion {
    region: RegionRef,
}

impl Drop for CGSRegion {
    fn drop(&mut self) {}
}

impl CGSRegion {
    #[inline]
    pub fn from_rect(_rect: &CGRect) -> CGSRegion {
        CGSRegion { region: ptr::null_mut() }
    }

    #[inline]
    pub fn from_rects(_rects: &[CGRect]) -> CGSRegion {
        CGSRegion { region: ptr::null_mut() }
    }
}

pub struct CGSSurface {
    _context_id: c_uint,
    _window_number: c_int,
    surface_id: c_uint,
}

impl CGSSurface {
    #[inline]
    pub fn from_ids(context_id: c_uint, window_number: c_int, surface_id: c_uint) -> CGSSurface {
        CGSSurface {
            _context_id: context_id,
            _window_number: window_number,
            surface_id,
        }
    }

    #[inline]
    pub fn id(&self) -> c_uint {
        self.surface_id
    }

    #[inline]
    pub fn set_shape(&self, _region: &CGSRegion) {}
}
