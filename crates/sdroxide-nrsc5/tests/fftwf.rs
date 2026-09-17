//! Checks the single-precision FFTW stand-in against a naive DFT.
//!
//! The stand-in is what makes the crate buildable without a system FFTW, and
//! a subtly wrong FFT would still produce plausible-looking noise that a
//! decode test can miss. This compares the two receive-path lengths (2048 in
//! FM, 256 in AM) against the definition, forward only — nrsc5 never asks for
//! an inverse transform.

use std::ffi::{c_int, c_uint, c_void};

// Keep the crate's rlib in the link so its native archive (which defines the
// fftwf_* symbols below) is forwarded to the linker.
use sdroxide_nrsc5 as _;

#[repr(C)]
#[derive(Clone, Copy)]
struct Complex {
    re: f32,
    im: f32,
}

unsafe extern "C" {
    fn fftwf_plan_dft_1d(
        n: c_int,
        input: *mut Complex,
        output: *mut Complex,
        sign: c_int,
        flags: c_uint,
    ) -> *mut c_void;
    fn fftwf_execute(plan: *mut c_void);
    fn fftwf_destroy_plan(plan: *mut c_void);
}

const FFTW_FORWARD: c_int = -1;
const FFTW_ESTIMATE: c_uint = 1 << 6;

/// A cheap deterministic PRNG so the test needs no dependency.
fn noise(n: usize, seed: u64) -> Vec<Complex> {
    let mut state = seed | 1;
    let mut next = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        ((state >> 11) as f64 / (1u64 << 53) as f64) - 0.5
    };
    (0..n)
        .map(|_| Complex {
            re: next() as f32,
            im: next() as f32,
        })
        .collect()
}

/// The DFT evaluated directly: O(n^2), exactly what the transform must match.
fn naive_dft(input: &[Complex]) -> Vec<Complex> {
    let n = input.len();
    let mut out = vec![Complex { re: 0.0, im: 0.0 }; n];
    for (k, o) in out.iter_mut().enumerate() {
        let mut acc = Complex { re: 0.0, im: 0.0 };
        for (j, x) in input.iter().enumerate() {
            let ang = -2.0 * std::f64::consts::PI * (k * j) as f64 / n as f64;
            let (s, c) = ang.sin_cos();
            acc.re += (x.re as f64 * c - x.im as f64 * s) as f32;
            acc.im += (x.re as f64 * s + x.im as f64 * c) as f32;
        }
        *o = acc;
    }
    out
}

fn check(n: usize) {
    let mut input = noise(n, 0x5d52_6f78_6964_65);
    let mut output = vec![Complex { re: 0.0, im: 0.0 }; n];
    let plan = unsafe {
        fftwf_plan_dft_1d(
            n as c_int,
            input.as_mut_ptr(),
            output.as_mut_ptr(),
            FFTW_FORWARD,
            FFTW_ESTIMATE,
        )
    };
    assert!(!plan.is_null(), "plan for n={n} failed");
    unsafe { fftwf_execute(plan) };
    unsafe { fftwf_destroy_plan(plan) };

    let expected = naive_dft(&input);

    let peak = expected.iter().fold(0.0f32, |m, c| m.max(c.re.abs()).max(c.im.abs()));
    let mut worst = 0.0f32;
    for (got, want) in output.iter().zip(&expected) {
        worst = worst.max((got.re - want.re).abs()).max((got.im - want.im).abs());
    }
    // Single precision over 2048 terms still leaves a wide margin here.
    assert!(
        worst <= peak * 1e-4,
        "n={n}: worst-bin error {worst} exceeds {:.3e}",
        peak * 1e-4
    );
}

#[test]
fn fm_length_matches_naive_dft() {
    check(2048);
}

#[test]
fn am_length_matches_naive_dft() {
    check(256);
}