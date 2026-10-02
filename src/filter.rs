//! rclone's exclude semantics, so the watcher never pushes what rclone itself
//! would have skipped (a Synology's `#recycle`, `@eaDir`, …).
//!
//! A port of `fs/filter/glob.go` and the exclude half of `filter.go`:
//!
//! * a glob without a leading `/` matches at any depth, with one means from the
//!   root;
//! * `*` stops at `/`, `**` does not, `?` is one non-`/` character;
//! * `[abc]`, `{a,b}` and `{{regexp}}` work as in rclone;
//! * a pattern ending in `/`, or containing `**`, also excludes whole
//!   directories, so everything beneath them is skipped. A plain pattern such
//!   as `.AppleDouble` matches only a path that *is* that name.
//!
//! The integration tests compare this against `rclone lsf --exclude`.

use anyhow::{Result, anyhow, bail};
use regex::Regex;

#[derive(Debug)]
pub struct Filter {
    file_rules: Vec<Regex>,
    dir_rules: Vec<Regex>,
}

impl Filter {
    pub fn new<S: AsRef<str>>(excludes: &[S]) -> Result<Self> {
        let mut f = Self {
            file_rules: Vec::new(),
            dir_rules: Vec::new(),
        };
        for glob in excludes {
            f.add_exclude(glob.as_ref())?;
        }
        Ok(f)
    }

    fn add_exclude(&mut self, glob: &str) -> Result<()> {
        let mut glob = glob.to_string();
        let is_dir_rule = glob.ends_with('/');
        // Excluding "dir/" is equivalent to excluding "dir/**".
        if is_dir_rule {
            glob.push_str("**");
        }
        let (mut dir_rule, mut file_rule) = (is_dir_rule, !is_dir_rule);
        if glob.contains("**") {
            dir_rule = true;
            file_rule = true;
        }
        let re = glob_to_regex(&glob).map_err(|e| anyhow!("exclude {glob:?}: {e}"))?;
        if file_rule {
            self.file_rules.push(re.clone());
        }
        if dir_rule {
            self.dir_rules.push(re);
        }
        Ok(())
    }

    /// Whether a file at `path` (relative to the sync root, `/`-separated)
    /// should be kept: neither the file nor any directory above it is excluded.
    pub fn includes_file(&self, path: &str) -> bool {
        if self.file_rules.iter().any(|r| r.is_match(path)) {
            return false;
        }
        let mut dir = path;
        while let Some(i) = dir.rfind('/') {
            dir = &dir[..i];
            if !self.includes_dir(dir) {
                return false;
            }
        }
        true
    }

    /// Whether the directory `path` should be kept.
    pub fn includes_dir(&self, path: &str) -> bool {
        // rclone tests directories with a trailing slash.
        let with_slash = format!("{}/", path.trim_matches('/'));
        !self.dir_rules.iter().any(|r| r.is_match(&with_slash))
    }
}

/// `GlobPathToRegexp(glob, ignoreCase=false)`.
fn glob_to_regex(glob: &str) -> Result<Regex> {
    let mut re = String::new();
    let glob = match glob.strip_prefix('/') {
        Some(rest) => {
            re.push('^');
            rest
        }
        None => {
            re.push_str("(^|/)");
            glob
        }
    };

    let mut stars = 0usize;
    let flush = |re: &mut String, stars: &mut usize| -> Result<()> {
        match *stars {
            0 => {}
            1 => re.push_str("[^/]*"),
            2 => re.push_str(".*"),
            _ => bail!("too many stars"),
        }
        *stars = 0;
        Ok(())
    };

    let (mut brace_depth, mut in_brackets) = (0i32, 0i32);
    let (mut slashed, mut in_regexp, mut in_regexp_end) = (false, false, false);
    let mut prev = '\0';
    for c in glob.chars() {
        let last = prev;
        prev = c;
        if slashed {
            re.push(c);
            slashed = false;
            continue;
        }
        if in_regexp_end {
            if c == '}' {
                // "}}}" ends the regexp with the longest segment: the final ')'
                // becomes '}' and a new ')' closes the group.
                re.pop();
                re.push('}');
                re.push(')');
                continue;
            }
            in_regexp_end = false;
        }
        if in_regexp {
            if c == '}' && last == '}' {
                in_regexp = false;
                in_regexp_end = true;
                // The first '}' was already written; turn it into the ')'.
                re.pop();
                re.push(')');
            } else {
                re.push(c);
            }
            continue;
        }
        if c != '*' {
            flush(&mut re, &mut stars)?;
        }
        if in_brackets > 0 {
            re.push(c);
            if c == '[' {
                in_brackets += 1;
            }
            if c == ']' {
                in_brackets -= 1;
            }
            continue;
        }
        match c {
            '\\' => {
                re.push(c);
                slashed = true;
            }
            '*' => stars += 1,
            '?' => re.push_str("[^/]"),
            '[' => {
                re.push(c);
                in_brackets += 1;
            }
            ']' => bail!("mismatched ']'"),
            '{' => {
                if brace_depth > 0 && last == '{' {
                    // "{{" starts a raw regexp; the '(' the first '{' wrote
                    // wraps it, so undo that brace's depth.
                    in_regexp = true;
                    brace_depth -= 1;
                } else {
                    brace_depth += 1;
                    re.push('(');
                }
            }
            '}' => {
                if brace_depth <= 0 {
                    bail!("mismatched '{{' and '}}'");
                }
                re.push(')');
                brace_depth -= 1;
            }
            ',' => re.push(if brace_depth > 0 { '|' } else { ',' }),
            '.' | '+' | '(' | ')' | '|' | '^' | '$' => {
                re.push('\\');
                re.push(c);
            }
            c => re.push(c),
        }
    }
    flush(&mut re, &mut stars)?;
    if in_brackets > 0 {
        bail!("mismatched '[' and ']'");
    }
    if brace_depth != 0 {
        bail!("mismatched '{{' and '}}'");
    }
    if in_regexp {
        bail!("mismatched '{{{{' and '}}}}'");
    }
    re.push('$');
    Regex::new(&re).map_err(|e| anyhow!("{e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn defaults() -> Filter {
        Filter::new(&[
            ".AppleDouble",
            ".DS_Store",
            ".Spotlight-V100",
            ".Trashes",
            "@eaDir/**",
            "#recycle/**",
            "$RECYCLE.BIN/**",
            "Thumbs.db",
        ])
        .unwrap()
    }

    #[test]
    fn module_defaults() {
        let f = defaults();
        for kept in ["a.txt", "Sub Dir/b.docx", "recycle/x", "notes/#recycle.txt"] {
            assert!(f.includes_file(kept), "{kept} should be kept");
        }
        for dropped in [
            "#recycle/old.docx",
            "#recycle/deep/er/x",
            "share/#recycle/x",
            "@eaDir/SYNOFILE_THUMB.jpg",
            "docs/@eaDir/x/y.jpg",
            "$RECYCLE.BIN/x",
            ".DS_Store",
            "docs/.DS_Store",
            "docs/Thumbs.db",
        ] {
            assert!(!f.includes_file(dropped), "{dropped} should be excluded");
        }
    }

    #[test]
    fn plain_names_match_only_that_exact_path() {
        // Like rclone: a file *inside* a directory called .AppleDouble is not
        // caught by the plain pattern (rclone would list it).
        let f = defaults();
        assert!(f.includes_file(".AppleDouble/inside.txt"));
        assert!(!f.includes_file(".AppleDouble"));
    }

    #[test]
    fn anchoring_and_stars() {
        let f = Filter::new(&["/root-only.txt", "*.tmp", "build/**", "a?c", "{x,y}.log"]).unwrap();
        assert!(!f.includes_file("root-only.txt"));
        assert!(f.includes_file("sub/root-only.txt"), "leading / anchors");
        assert!(!f.includes_file("any/where/scratch.tmp"));
        assert!(!f.includes_file("build/out/o.bin"));
        assert!(!f.includes_file("abc"));
        assert!(f.includes_file("a/c"), "? does not cross /");
        assert!(!f.includes_file("y.log"));
        assert!(f.includes_file("z.log"));
    }

    #[test]
    fn trailing_slash_excludes_the_directory_tree() {
        let f = Filter::new(&["cache/"]).unwrap();
        assert!(!f.includes_file("cache/a"));
        assert!(!f.includes_file("x/cache/a/b"));
        assert!(f.includes_file("cache.txt"));
        assert!(!f.includes_dir("cache"));
    }

    #[test]
    fn regexp_sections() {
        let f = Filter::new(&["{{.*\\.bak[0-9]}}"]).unwrap();
        assert!(!f.includes_file("old.bak1"));
        assert!(f.includes_file("old.bak"));
    }

    #[test]
    fn bad_globs_are_errors_not_panics() {
        for bad in ["[abc", "abc]", "{a,b", "a}", "***"] {
            assert!(Filter::new(&[bad]).is_err(), "{bad:?}");
        }
    }
}
