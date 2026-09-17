//! The one faad2 in the binary: knik0's 2.11.2 with nrsc5's HDC patch applied
//! at build time, compiled with both `DRM_SUPPORT` and `HDC_SUPPORT`.
//!
//! There is no Rust API. `sdroxide-drm` (Dream) and `sdroxide-nrsc5` both call
//! faad2 from their own C, compile against the patched headers this crate's
//! build script exports as `DEP_FAAD2_INCLUDE`, and name this crate in their
//! `lib.rs` so its archive is linked. `links = "faad2"` is what keeps it to one:
//! two faad2 archives would collide on every `NeAACDec*` symbol. See
//! `build.rs` for why the patch is applied here rather than to a fork.

#[cfg(test)]
mod patch;

#[cfg(test)]
mod tests {
    use super::patch::{apply_to_text, parse};

    const SAMPLE: &str = "\
From 0000 Mon Sep 17 00:00:00 2001
Subject: [PATCH] sample

---
 a.c | 3 ++-

diff --git a/a.c b/a.c
index 1..2 100644
--- a/a.c
+++ b/a.c
@@ -1,3 +1,4 @@
 one
-two
+TWO
+two and a half
 three
@@ -5,0 +7,1 @@
+appended
--
2.43.0
";

    #[test]
    fn hunks_are_read_by_their_counts_not_their_first_character() {
        let files = parse(SAMPLE).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, "a.c");
        assert_eq!(files[0].hunks.len(), 2);
        // The mail signature after the last hunk starts with `-` and must not
        // have been read as a removal.
        assert_eq!(files[0].hunks[1].old, Vec::<String>::new());
        assert_eq!(files[0].hunks[1].new, vec!["appended".to_string()]);
    }

    #[test]
    fn a_patch_applies_where_it_says_and_keeps_the_final_newline() {
        let files = parse(SAMPLE).unwrap();
        let out = apply_to_text("one\ntwo\nthree\nfour\nfive\n", &files[0]).unwrap();
        assert_eq!(out, "one\nTWO\ntwo and a half\nthree\nfour\nfive\nappended\n");
    }

    #[test]
    fn a_hunk_that_does_not_fit_stops_rather_than_half_applying() {
        let files = parse(SAMPLE).unwrap();
        let err = apply_to_text("one\n2\nthree\nfour\nfive\n", &files[0]).unwrap_err();
        assert!(err.contains("does not match"), "{err}");
    }

    /// Only the files that were copied are patched; the ones the patch names
    /// outside that copy (faad2's CMakeLists.txt, its frontend) are handed back.
    #[test]
    fn files_outside_the_copied_tree_are_reported_not_invented() {
        let root =
            std::env::temp_dir().join(format!("sdroxide-faad2-patch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("a.c"), "one\ntwo\nthree\nfour\nfive\n").unwrap();
        let other = SAMPLE.replace("a.c", "frontend/main.c");
        let skipped = super::patch::apply_to_tree(&root, &format!("{SAMPLE}{other}")).unwrap();
        assert_eq!(skipped, vec!["frontend/main.c".to_string()]);
        assert!(std::fs::read_to_string(root.join("a.c")).unwrap().ends_with("appended\n"));
        assert!(!root.join("frontend").exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn parts_it_cannot_apply_are_refused() {
        let new_file = "diff --git a/b.c b/b.c\nnew file mode 100644\n--- /dev/null\n+++ b/b.c\n";
        assert!(parse(new_file).is_err());
    }

    /// The real pair: nrsc5's patch onto the pinned faad2. Every hunk has to
    /// land exactly, and the result has to declare the HDC entry point nrsc5
    /// calls. Skipped when the submodules are not checked out.
    #[test]
    fn nrsc5s_hdc_patch_fits_the_pinned_faad2() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../vendor");
        let Ok(patch) = std::fs::read_to_string(root.join("nrsc5/support/faad2-hdc-support.patch"))
        else {
            eprintln!("vendor/nrsc5 not checked out; skipping");
            return;
        };
        let files = parse(&patch).unwrap();
        let mut applied = 0;
        for file in &files {
            let Ok(text) = std::fs::read_to_string(root.join("faad2").join(&file.path)) else {
                panic!("the patch names {}, which faad2 does not have", file.path);
            };
            let patched = apply_to_text(&text, file).unwrap();
            if file.path == "include/neaacdec.h" {
                assert!(patched.contains("NeAACDecInitHDC"), "the HDC entry point is declared");
            }
            applied += file.hunks.len();
        }
        assert_eq!(applied, 46, "every hunk of the 15-file patch");
    }
}
