//! Builds the vendored nrsc5 HD Radio library and the single-precision FFTW
//! stand-in it needs, both straight with `cc` rather than nrsc5's CMake.
//!
//! The `HAVE_*` CFLAGS below select what upstream's `configure_file` would
//! have guessed. What matters is the shape of the build:
//!
//! * **nrsc5** — `vendor/nrsc5` is a GPL-3.0-or-later submodule (see the
//!   package manifest for where that sits licence-wise). The CLI (`main.c`) is
//!   not built; the library itself and `rtltcp.c` are. Its own CMake only
//!   exists to wire up fftw3f and faad2, neither of which is usable here.
//!   `src/rtlsdr_stubs.c` supplies no-ops, under private names, for the
//!   librtlsdr calls the library makes on its device path, which the pipe path
//!   never reaches.
//! * **fftw3f** — `include/fftw3.h` plus `src/fftwf_compat.c` provide the five
//!   single-precision entry points the library uses. Every transform the
//!   receive path creates is a power of two (2048 in FM, 256 in AM), so the
//!   stand-in is a plain radix-2 Cooley-Tukey and needs none of Dream's
//!   Bluestein fallback.
//! * **faad2** — nrsc5 decodes its HDC audio with `NeAACDec*`, from the one
//!   DRM+HDC faad2 archive `crates/sdroxide-faad2` builds with nrsc5's HDC
//!   patch applied. The include path is that crate's patched headers
//!   (`DEP_FAAD2_INCLUDE`), so `NeAACDecInitHDC` is declared; the unpatched
//!   `vendor/faad2` does not have it. A second faad2 copy must never be
//!   compiled in.
//!
//! Opened on a pipe, the library runs no thread of its own: all of its work and
//! every callback happen inside the call that pipes samples in, which is why
//! `src/worker.rs` makes that call from a thread of its own. The Windows CI
//! build is MinGW, like nrsc5's own `msys2-build`; MSVC cannot compile this C
//! at all.

use std::env;
use std::fs;
use std::path::PathBuf;

/// nrsc5's `LIBRARY_FILES` in `src/CMakeLists.txt`: the library plus its
/// rtl_tcp transport, minus the CLI.
const NRS_LIBRARY_SOURCES: &[&str] = &[
    "acquire.c",
    "decode.c",
    "frame.c",
    "here_images.c",
    "input.c",
    "nrsc5.c",
    "output.c",
    "pids.c",
    "rtltcp.c",
    "sync.c",
    "firdecim_cf32.c",
    "conv_dec.c",
    "rs_init.c",
    "rs_decode.c",
    "unicode.c",
    "strndup.c",
];

/// What nrsc5's `config.h.in` generates via CMake. `HAVE_CMPLXF` leans on the
/// `CMPLXF()` macro of `<complex.h>`, which GCC/Clang/MinGW provide in their
/// gnu11 modes (this build forces `-std=gnu11`, like nrsc5's own CMake).
/// `strndup` needs `_GNU_SOURCE` on glibc, which the compile defines below.
const CONFIG_H: &str = r#"#pragma once

/* nrsc5's CMake defines both of these when faad2 is found: `HAVE_FAAD2` gates
 * the decoder declarations in the headers, `USE_FAAD2` the decode calls. */
#define HAVE_FAAD2 1
#define USE_FAAD2 1

#define HAVE_STRNDUP 1
#define HAVE_CMPLXF 1
#define HAVE_COMPLEX_I 1

/* `log_debug` fires at 1 and below, `log_info` at 2, `log_warn` at 3 and
 * `log_error` at 4 (`src/defines.h`). `4` keeps only the errors on stderr: a
 * weak station makes the decoder warn about every packet it cannot decode,
 * and the panel already shows that as CBER and the AUDIO light. */
#define LIBRARY_DEBUG_LEVEL 4
"#;

fn main() {
    // A build script runs on the host, so the target comes from the environment.
    let os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let target_env = env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    if target_env == "msvc" {
        panic!(
            "nrsc5 is C99 with complex arithmetic, which MSVC cannot compile; build sdroxide \
             for the x86_64-pc-windows-gnu target (MinGW), as the Windows release does"
        );
    }

    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let out = PathBuf::from(env::var("OUT_DIR").unwrap());

    let nrsc5 = manifest.join("../../vendor/nrsc5");
    // The patched faad2 headers, exported by `sdroxide-faad2`'s build script.
    let faad2_include = PathBuf::from(
        env::var("DEP_FAAD2_INCLUDE").expect("sdroxide-faad2 exports its include directory"),
    );
    if !nrsc5.join("include/nrsc5.h").exists() {
        panic!(
            "vendored nrsc5 is missing at {}\nSynchronise submodules: git submodule update --init --recursive",
            nrsc5.display()
        );
    }

    println!("cargo:rerun-if-changed=src/fftwf_compat.c");
    println!("cargo:rerun-if-changed=src/rtlsdr_stubs.c");
    println!("cargo:rerun-if-changed=src/layout_check.c");
    println!("cargo:rerun-if-changed=include");
    println!("cargo:rerun-if-changed={}", nrsc5.join("src").display());
    println!("cargo:rerun-if-changed={}", nrsc5.join("include").display());

    fs::write(out.join("config.h"), CONFIG_H).expect("write config.h");

    let mut build = cc::Build::new();
    build
        .include(&out)
        .include(nrsc5.join("src"))
        .include(nrsc5.join("include"))
        .include(manifest.join("include"))
        .include(&faad2_include)
        .define("_GNU_SOURCE", None)
        .define("GIT_COMMIT_HASH", "\"0225922\"")
        .opt_level(2)
        .warnings(false);

    // nrsc5's CMake forces this too.
    build.flag("-std=gnu11");

    for src in NRS_LIBRARY_SOURCES {
        build.file(nrsc5.join("src").join(src));
    }
    // nrsc5.c calls librtlsdr unconditionally; these stubs stand in for it on
    // the pipe-only path (see the file's comment).
    build.file(manifest.join("src/rtlsdr_stubs.c"));
    build.file(manifest.join("src/fftwf_compat.c"));
    // Compile-time only: fails the build if `nrsc5_event_t` no longer matches
    // what `src/lib.rs` reads out of it.
    build.file(manifest.join("src/layout_check.c"));
    build.compile("sdroxide_nrsc5");

    // nrsc5 is built with pthread (its device-path worker) and rtltcp's sockets.
    match os.as_str() {
        "linux" | "macos" | "freebsd" => {
            println!("cargo:rustc-link-lib=dylib=pthread");
            println!("cargo:rustc-link-lib=dylib=m");
        }
        "windows" => {
            println!("cargo:rustc-link-lib=dylib=ws2_32");
        }
        _ => {}
    }
}