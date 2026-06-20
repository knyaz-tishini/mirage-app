//! BloomState — eased scalars that drive the bloom animation.
//!
//! Ported from `mirage-desktop-client.html` (JS variables `flowerT`, `glowI`,
//! `stemT`, `trem`, `spd`, `eNorm`, `todLight`).

use crate::centerpiece::ConnPhase;

// ── Constants ─────────────────────────────────────────────────────────────────

/// ihash seed matching the JS source (`seedi = 987654`).
pub const SEED: u32 = 987_654;

/// Number of petal rings.
pub const RINGS: usize = 6;

/// Outermost petal colour RGB.
pub const OUTER: [u8; 3] = [255, 226, 190];
/// Mid-ring petal colour RGB.
pub const INNER: [u8; 3] = [251, 168, 96];
/// Inner-ring petal colour RGB.
pub const DEEP: [u8; 3] = [206, 116, 52];
/// Core circle colour RGB.
pub const CORE_C: [u8; 3] = [120, 66, 30];
/// Night tint colour RGB (applied at low `tod_light`).
pub const NIGHT_TINT: [u8; 3] = [70, 86, 120];

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Lerp an `f64` toward `tgt` at rate `r` per second.
pub fn ez(x: f64, tgt: f64, r: f64, dt: f64) -> f64 {
    x + (tgt - x) * (r * dt).min(1.0)
}

/// Smoothstep `u` (clamp-free — caller ensures 0 ≤ u ≤ 1).
pub fn smooth(u: f64) -> f64 {
    u * u * (3.0 - 2.0 * u)
}

/// Integer hash matching the JS `ihash(i, j, s)` function.
///
/// Returns a deterministic pseudo-random value in `[0, 1)` for the given
/// (`i`, `j`, `seed`) triple.
pub fn ihash(i: u32, j: u32, seed: u32) -> f64 {
    let h = (i.wrapping_mul(374_761_393))
        .wrapping_add(j.wrapping_mul(668_265_263))
        .wrapping_add(seed);
    let h = (h ^ (h >> 13)).wrapping_mul(1_274_126_177);
    let h = h ^ (h >> 16);
    h as f64 / 4_294_967_296.0
}

/// Bloom open angle in degrees driven by the local hour of day.
///
/// Matches JS `spreadDeg(h)`.  Idle/disconnected bloom stays at 360°.
pub fn spread_deg(hour: f64) -> f64 {
    if hour >= 8.0 && hour <= 16.0 {
        360.0
    } else if hour >= 23.0 || hour <= 1.0 {
        60.0
    } else if hour > 16.0 && hour < 23.0 {
        360.0 + (60.0 - 360.0) * smooth((hour - 16.0) / 7.0)
    } else {
        60.0 + (360.0 - 60.0) * smooth((hour - 1.0) / 7.0)
    }
}

/// Daylight factor 0..1: peak ≈ 13:00, trough ≈ 01:00.
pub fn day_factor(hour: f64) -> f64 {
    0.12 + 0.88 * ((((hour - 13.0) / 24.0) * std::f64::consts::TAU).cos() + 1.0) / 2.0
}

/// Apply night tint to an RGB colour.
///
/// Matches JS `tint(c)`: lerp toward NIGHT_TINT weighted by `(1 - tod_light)
/// * 0.4`, then scale brightness by `0.55 + 0.45 * tod_light`.
pub fn tint_color(c: [u8; 3], tod_light: f64) -> [u8; 3] {
    let lerp = |a: u8, b: u8, t: f64| -> u8 { (a as f64 + (b as f64 - a as f64) * t) as u8 };
    let t = (1.0 - tod_light) * 0.4;
    let m = [
        lerp(c[0], NIGHT_TINT[0], t),
        lerp(c[1], NIGHT_TINT[1], t),
        lerp(c[2], NIGHT_TINT[2], t),
    ];
    let b = 0.55 + 0.45 * tod_light;
    [(m[0] as f64 * b) as u8, (m[1] as f64 * b) as u8, (m[2] as f64 * b) as u8]
}

/// Lerp two RGB colours.
pub fn lerp_color(a: [u8; 3], b: [u8; 3], t: f64) -> [u8; 3] {
    [
        (a[0] as f64 + (b[0] as f64 - a[0] as f64) * t) as u8,
        (a[1] as f64 + (b[1] as f64 - a[1] as f64) * t) as u8,
        (a[2] as f64 + (b[2] as f64 - a[2] as f64) * t) as u8,
    ]
}

// ── BloomState ────────────────────────────────────────────────────────────────

/// All eased scalars that drive the bloom.
pub struct BloomState {
    /// Accumulated time in seconds (same as `T` in JS).
    pub t: f64,
    /// Flower open factor: 0 = closed, 1 = fully open.
    pub flower_t: f64,
    /// Glow intensity: 0 = no glow, 1 = full glow.
    pub glow_i: f64,
    /// Stem emergence: 0 = hidden, 1 = fully visible.
    pub stem_t: f64,
    /// Tremble (jitter) amount: 0 = still.
    pub trem: f64,
    /// Spread angle in degrees.
    pub spd: f64,
    /// Normalised spread: 0 = tightest, 1 = widest.
    pub e_norm: f64,
    /// Time-of-day light factor: 0 = night, 1 = peak day.
    pub tod_light: f64,
}

impl BloomState {
    pub fn new() -> Self {
        Self {
            t: 0.0,
            flower_t: 0.0,
            glow_i: 0.0,
            stem_t: 0.0,
            trem: 0.0,
            spd: 360.0,
            e_norm: 1.0,
            tod_light: 1.0,
        }
    }

    /// Advance all eased state by one frame.
    ///
    /// `phase` is the authoritative connection phase.
    /// `hour_frac` is the current local hour (e.g. 14.5 for 14:30).
    pub fn tick(&mut self, dt: f64, phase: ConnPhase, hour_frac: f64) {
        self.t += dt;

        let connected = matches!(phase, ConnPhase::Connected | ConnPhase::Disconnecting);
        let target_flower = if connected { 1.0 } else { 0.0 };

        self.flower_t = ez(self.flower_t, target_flower, 2.5, dt);

        // Time-of-day drives petal warmth.
        self.tod_light = ez(self.tod_light, day_factor(hour_frac), 0.5, dt);

        // Idle bloom stays fully spread; connected bloom follows local hour.
        let target_spd = if connected { spread_deg(hour_frac) } else { 360.0 };
        self.spd = ez(self.spd, target_spd, 2.5, dt);
        self.e_norm = ((self.spd - 60.0) / 300.0).clamp(0.0, 1.0);

        self.glow_i = ez(self.glow_i, if connected { 1.0 } else { 0.0 }, 3.0, dt);
        // Stem grows once leaving the off state (reference: stemT → state>=1).
        let has_stem = !matches!(phase, ConnPhase::Disconnected);
        self.stem_t = ez(self.stem_t, if has_stem { 1.0 } else { 0.0 }, 1.8, dt);
        // Bud tremble during the connecting handshake (reference: trem → state===1).
        let tremble = matches!(phase, ConnPhase::Connecting);
        self.trem = ez(self.trem, if tremble { 1.0 } else { 0.0 }, 3.0, dt);
    }
}

impl Default for BloomState {
    fn default() -> Self {
        Self::new()
    }
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bloom_tick_connects() {
        let mut s = BloomState::new();
        for _ in 0..200 {
            s.tick(0.01, ConnPhase::Connected, 12.0);
        }
        assert!(s.flower_t > 0.9, "flower_t should open after 2s: {}", s.flower_t);
    }

    #[test]
    fn test_bloom_tick_disconnects() {
        let mut s = BloomState::new();
        // Start connected.
        for _ in 0..200 {
            s.tick(0.01, ConnPhase::Connected, 12.0);
        }
        assert!(s.flower_t > 0.9);
        // Disconnect.
        for _ in 0..200 {
            s.tick(0.01, ConnPhase::Disconnected, 12.0);
        }
        assert!(s.flower_t < 0.1, "flower_t should close after 2s: {}", s.flower_t);
    }

    #[test]
    fn test_tod_night() {
        let mut s = BloomState::new();
        // Advance at hour = 2 (deep night).
        for _ in 0..500 {
            s.tick(0.01, ConnPhase::Disconnected, 2.0);
        }
        // spd stays at 360 when disconnected regardless of hour.
        assert!(s.spd > 350.0, "idle bloom must stay fully spread: {}", s.spd);
    }

    #[test]
    fn test_ihash_determinism() {
        let a = ihash(3, 7, SEED);
        let b = ihash(3, 7, SEED);
        assert_eq!(a, b, "ihash must be deterministic");
        // Different inputs must differ.
        assert_ne!(ihash(3, 7, SEED), ihash(4, 7, SEED));
    }
}
