//! Apply a `git format-patch` / `git diff` patch to a tree of text files.
//!
//! Just enough of `git apply` for one job: putting nrsc5's HDC patch onto the
//! pinned faad2 at build time, on every platform the release builds for, with
//! nothing installed — no `git`, no `patch`, which a Windows build host or a
//! source tarball cannot be assumed to have. Shared between `build.rs`, which
//! applies it, and the crate's tests, which pin it.
//!
//! What it handles is what that patch contains: modified text files, LF line
//! endings, unified hunks. What it refuses rather than guesses at is anything
//! else — a new, deleted or renamed file, a binary part, a hunk whose context
//! is not in the file. A patch that no longer fits the tree has to stop the
//! build, not produce a decoder that is quietly half patched.

use std::path::Path;

/// One file's worth of a patch.
#[derive(Debug, PartialEq)]
pub struct FilePatch {
    /// The path relative to the tree root, as the patch names it after `b/`.
    pub path: String,
    pub hunks: Vec<Hunk>,
}

/// One `@@` block.
#[derive(Debug, PartialEq)]
pub struct Hunk {
    /// 1-based line in the original file where the old lines start.
    pub old_start: usize,
    /// The lines the hunk expects to find: context and removals.
    pub old: Vec<String>,
    /// What replaces them: context and additions.
    pub new: Vec<String>,
}

/// Split a patch into its files and hunks.
///
/// Hunk bodies are read by their declared line counts rather than by the first
/// character of each line, because a mail-formatted patch ends in a `-- `
/// signature line that would otherwise read as a removal.
pub fn parse(patch: &str) -> Result<Vec<FilePatch>, String> {
    let mut files: Vec<FilePatch> = Vec::new();
    let mut lines = patch.lines().peekable();
    while let Some(line) = lines.next() {
        if line.starts_with("diff --git ") {
            files.push(FilePatch { path: String::new(), hunks: Vec::new() });
            continue;
        }
        let Some(file) = files.last_mut() else { continue };
        for refused in ["new file mode", "deleted file mode", "rename from", "Binary files"] {
            if line.starts_with(refused) {
                return Err(format!("unsupported patch part ({refused}) in {}", file.path));
            }
        }
        if let Some(path) = line.strip_prefix("+++ ") {
            let path = path.trim_end();
            file.path = path.strip_prefix("b/").unwrap_or(path).to_string();
            continue;
        }
        let Some(header) = line.strip_prefix("@@ -") else { continue };
        let (old_start, old_len, new_len) = hunk_header(header)
            .ok_or_else(|| format!("malformed hunk header `{line}` in {}", file.path))?;
        let mut hunk = Hunk { old_start, old: Vec::new(), new: Vec::new() };
        while hunk.old.len() < old_len || hunk.new.len() < new_len {
            let body =
                lines.next().ok_or_else(|| format!("patch ends inside a hunk of {}", file.path))?;
            // A context line for an empty source line may have lost its leading
            // space to an editor or a mailer.
            let (tag, text) = match body.chars().next() {
                Some(c @ (' ' | '-' | '+')) => (c, &body[1..]),
                None => (' ', ""),
                Some('\\') => continue, // "\ No newline at end of file"
                Some(_) => return Err(format!("unexpected line `{body}` in {}", file.path)),
            };
            match tag {
                ' ' => {
                    hunk.old.push(text.to_string());
                    hunk.new.push(text.to_string());
                }
                '-' => hunk.old.push(text.to_string()),
                _ => hunk.new.push(text.to_string()),
            }
        }
        if hunk.old.len() != old_len || hunk.new.len() != new_len {
            return Err(format!("hunk at line {old_start} of {} miscounts its lines", file.path));
        }
        file.hunks.push(hunk);
    }
    files.retain(|f| !f.hunks.is_empty());
    if let Some(f) = files.iter().find(|f| f.path.is_empty()) {
        return Err(format!("a file in the patch has no `+++` path ({} hunks)", f.hunks.len()));
    }
    Ok(files)
}

/// `-12,7 +12,8 @@ …` → (12, 7, 8). A missing count is one line.
fn hunk_header(header: &str) -> Option<(usize, usize, usize)> {
    let (ranges, _) = header.split_once(" @@")?;
    let (old, new) = ranges.split_once(" +")?;
    let range = |r: &str| -> Option<(usize, usize)> {
        match r.split_once(',') {
            Some((start, len)) => Some((start.parse().ok()?, len.parse().ok()?)),
            None => Some((r.parse().ok()?, 1)),
        }
    };
    let (old_start, old_len) = range(old)?;
    let (_, new_len) = range(new)?;
    Some((old_start, old_len, new_len))
}

/// Apply one file's hunks to its text.
///
/// Each hunk must match exactly where the patch says, after the shift the
/// hunks above it caused. `git apply` would search further afield; a patch
/// pinned to a pinned tree has no reason to need that, and a hunk that has
/// moved means the pins have come apart.
pub fn apply_to_text(text: &str, file: &FilePatch) -> Result<String, String> {
    let ends_with_newline = text.ends_with('\n');
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    let mut shift: isize = 0;
    for hunk in &file.hunks {
        // A pure insertion names the line *before* it, so its start is one
        // lower than the index it lands at.
        let base = if hunk.old.is_empty() { hunk.old_start } else { hunk.old_start - 1 };
        let at = (base as isize + shift) as usize;
        let fits = lines.get(at..at + hunk.old.len()).is_some_and(|found| found == hunk.old);
        if !fits {
            return Err(format!(
                "hunk at line {} of {} does not match the file — the patch and the tree it \
                 was written for have come apart",
                hunk.old_start, file.path
            ));
        }
        lines.splice(at..at + hunk.old.len(), hunk.new.iter().cloned());
        shift += hunk.new.len() as isize - hunk.old.len() as isize;
    }
    let mut out = lines.join("\n");
    if ends_with_newline {
        out.push('\n');
    }
    Ok(out)
}

/// Apply every file in `patch` found under `root`, rewriting them in place.
/// Files the patch names that are not under `root` are skipped and returned —
/// the caller copies only the part of a tree it builds.
pub fn apply_to_tree(root: &Path, patch: &str) -> Result<Vec<String>, String> {
    let mut skipped = Vec::new();
    for file in parse(patch)? {
        let path = root.join(&file.path);
        if !path.exists() {
            skipped.push(file.path);
            continue;
        }
        let text = std::fs::read_to_string(&path)
            .map_err(|e| format!("reading {}: {e}", path.display()))?;
        let patched = apply_to_text(&text, &file)?;
        std::fs::write(&path, patched).map_err(|e| format!("writing {}: {e}", path.display()))?;
    }
    Ok(skipped)
}
