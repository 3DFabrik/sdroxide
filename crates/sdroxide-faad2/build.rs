//! Builds faad2 once for the whole binary: knik0's 2.11.2 from `vendor/faad2`,
//! with nrsc5's HDC patch applied to a copy of it at build time.
//!
//! Two decoders need faad2 and they need it in two variants. Dream decodes DRM
//! audio through `DRM_SUPPORT`; nrsc5 decodes HD Radio audio through the HDC
//! variant, which upstream faad2 does not carry — nrsc5 ships it as
//! `support/faad2-hdc-support.patch` and builds its own faad2 with it. Both in
//! one archive is the only arrangement that links: two faad2 copies collide on
//! every `NeAACDec*` symbol. The patch switches the HDC paths on only under
//! `HDC_SUPPORT`, and its DRM changes were checked byte-identical against a
//! real DRM broadcast in PR #466.
//!
//! The patch is applied here rather than committed into a fork of faad2 so the
//! submodule stays on upstream: the pin is knik0's, the patch is nrsc5's and
//! moves with the nrsc5 pin, and nothing depends on a third repository staying
//! up. The copy lives in `OUT_DIR`, so the submodule's working tree is never
//! touched and `git status` stays clean. See `src/patch.rs` for why the patch
//! is applied in Rust rather than by a `git` or `patch` found on the host.
//!
//! Dependents find the patched headers through `DEP_FAAD2_INCLUDE`.

#[path = "src/patch.rs"]
#[allow(dead_code)]
mod patch;

use std::path::{Path, PathBuf};

fn main() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let faad2 = manifest.join("../../vendor/faad2");
    let nrsc5 = manifest.join("../../vendor/nrsc5");
    let patch_file = nrsc5.join("support/faad2-hdc-support.patch");

    if !faad2.join("include/neaacdec.h").exists() {
        panic!(
            "vendored faad2 is missing at {}\nrun: git submodule update --init --recursive",
            faad2.display()
        );
    }
    if !patch_file.exists() {
        panic!(
            "nrsc5's faad2 HDC patch is missing at {} — the vendor/nrsc5 submodule is not \
             checked out\nrun: git submodule update --init --recursive",
            patch_file.display()
        );
    }

    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src/patch.rs");
    println!("cargo:rerun-if-changed={}", faad2.join("include").display());
    println!("cargo:rerun-if-changed={}", faad2.join("libfaad").display());
    println!("cargo:rerun-if-changed={}", patch_file.display());

    // Only the part of the tree that is compiled. The patch also touches
    // faad2's CMakeLists.txt and its command-line frontend, neither of which
    // this build uses.
    let tree = out.join("faad2");
    if tree.exists() {
        std::fs::remove_dir_all(&tree).expect("clear the previous patched faad2 copy");
    }
    for dir in ["include", "libfaad"] {
        copy_dir(&faad2.join(dir), &tree.join(dir));
    }
    let patch = std::fs::read_to_string(&patch_file).expect("read the faad2 HDC patch");
    let skipped = patch::apply_to_tree(&tree, &patch)
        .unwrap_or_else(|e| panic!("applying {}: {e}", patch_file.display()));
    for path in &skipped {
        assert!(
            !path.starts_with("include/") && !path.starts_with("libfaad/"),
            "the HDC patch names {path}, which the copied tree should have had"
        );
    }

    build(&tree);
    println!("cargo:include={}", tree.join("include").display());
}

/// The faad2 build both decoders link: the stock sources with `DRM_SUPPORT`,
/// which brings in `NeAACDecInitDRM` (upstream ships that as a second library,
/// `libfaad_drm`, because the plain one cannot decode DRM at all), and
/// `HDC_SUPPORT`, which brings in `NeAACDecInitHDC`.
fn build(tree: &Path) {
    let mut build = cc::Build::new();
    build
        .include(tree.join("libfaad"))
        .include(tree.join("include"))
        .define("HAVE_INTTYPES_H", "1")
        .define("HAVE_MEMCPY", "1")
        .define("HAVE_STRING_H", "1")
        .define("HAVE_STRINGS_H", "1")
        .define("HAVE_SYS_STAT_H", "1")
        .define("HAVE_SYS_TYPES_H", "1")
        .define("PACKAGE_VERSION", "\"2.11.2\"")
        .define("APPLY_DRC", None)
        .define("DRM_SUPPORT", None)
        .define("HDC_SUPPORT", None)
        .opt_level(2)
        .warnings(false);
    // The target's C library, not the host's: a build script runs on the host.
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() != Ok("msvc") {
        build.define("HAVE_LRINTF", "1");
    }
    let mut sources: Vec<PathBuf> = std::fs::read_dir(tree.join("libfaad"))
        .expect("read faad2/libfaad")
        .map(|entry| entry.expect("faad2 dir entry").path())
        .filter(|path| path.extension().is_some_and(|e| e == "c"))
        .collect();
    sources.sort();
    build.files(sources);
    build.compile("sdroxide_faad2");
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap_or_else(|e| panic!("create {}: {e}", to.display()));
    for entry in std::fs::read_dir(from).unwrap_or_else(|e| panic!("read {}: {e}", from.display()))
    {
        let entry = entry.expect("faad2 dir entry");
        let (src, dst) = (entry.path(), to.join(entry.file_name()));
        if entry.file_type().expect("faad2 file type").is_dir() {
            copy_dir(&src, &dst);
        } else {
            std::fs::copy(&src, &dst)
                .unwrap_or_else(|e| panic!("copy {} → {}: {e}", src.display(), dst.display()));
        }
    }
}
