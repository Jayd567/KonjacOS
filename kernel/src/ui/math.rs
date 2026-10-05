//! The handful of float helpers the desktop needs. `core` has no `sqrt`
//! (it lives in `std`, backed by libm), so it comes straight from the SSE
//! instruction instead -- SSE is always on here (see `boot.rs`).

use core::arch::x86_64::{_mm_cvtss_f32, _mm_set_ss, _mm_sqrt_ss};

#[inline]
pub fn sqrt(x: f32) -> f32 {
    #[allow(unused_unsafe)]
    unsafe {
        _mm_cvtss_f32(_mm_sqrt_ss(_mm_set_ss(x)))
    }
}

#[inline]
pub fn smoothstep(e0: f32, e1: f32, x: f32) -> f32 {
    let t = ((x - e0) / (e1 - e0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// A cheap, stable per-pixel hash for the glass's dither noise. Stable
/// (keyed only on screen position) so the grain doesn't shimmer as panels
/// redraw.
#[inline]
pub fn hash2(x: i32, y: i32) -> u32 {
    let mut h = (x as u32).wrapping_mul(0x8da6_b343) ^ (y as u32).wrapping_mul(0xd816_3841);
    h ^= h >> 13;
    h = h.wrapping_mul(0x5bd1_e995);
    h ^ (h >> 15)
}

/// A damped spring (Hooke's law, F = -kx - cv), integrated one 100 Hz
/// timer tick at a time. Underdamped on purpose: menus overshoot their
/// final size slightly and settle back, rather than easing in linearly.
#[derive(Clone, Copy)]
pub struct Spring {
    pub value: f32,
    pub velocity: f32,
    pub target: f32,
}

impl Spring {
    pub const fn new(value: f32) -> Self {
        Spring { value, velocity: 0.0, target: value }
    }

    pub fn step(&mut self, stiffness: f32, damping: f32) {
        const DT: f32 = 1.0 / 100.0;
        // Two half-steps per tick keeps the semi-implicit Euler stable at
        // the stiffness values used here.
        for _ in 0..2 {
            let force = -stiffness * (self.value - self.target) - damping * self.velocity;
            self.velocity += force * DT * 0.5;
            self.value += self.velocity * DT * 0.5;
        }
    }

    pub fn settled(&self) -> bool {
        (self.value - self.target).abs() < 0.002 && self.velocity.abs() < 0.01
    }

    pub fn snap(&mut self, v: f32) {
        self.value = v;
        self.target = v;
        self.velocity = 0.0;
    }
}
