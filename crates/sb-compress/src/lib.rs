//! RTK-style tool-result compression — a request-side compressor that shrinks
//! the bulky tool-result text in the message history before dispatch (git diffs,
//! grep/find dumps, build logs). It is the one 9router differentiator worth
//! stealing, ported with its safety philosophy intact.
//!
//! **Fail-safe by construction** (the part the deconstruction says to port
//! verbatim):
//! 1. **Never grow** — if a filter's output is >= the input, keep the input.
//! 2. **Never empty** — if a filter returns empty, keep the input.
//! 3. **`catch_unwind` passthrough** — if a filter panics, keep the input. A
//!    broken filter degrades to a no-op; it can never corrupt a request.
//! 4. **Skip error traces** — `is_error` tool results are high-signal; left
//!    untouched.
//! 5. **Size gates** — only blobs in `[MIN_COMPRESS_BYTES, MAX_COMPRESS_BYTES]`
//!    are touched; savings are byte-measured (no inflated headline).
//!
//! Detection is an ordered cascade of line-shape heuristics over the head of the
//! blob (regex-free). Mis-detection is bounded by the never-grow guard.

use sb_core::{AiRequest, ContentPart};

/// Tool results below this many bytes aren't worth compressing.
pub const MIN_COMPRESS_BYTES: usize = 500;
/// Tool results above this are left alone (pathological inputs).
pub const MAX_COMPRESS_BYTES: usize = 10 * 1024 * 1024;

/// Byte-measured outcome of compressing one request.
#[derive(Debug, Default, Clone)]
pub struct CompressionStats {
    pub bytes_before: usize,
    pub bytes_after: usize,
    /// Which filters fired (one entry per compressed blob).
    pub filters_applied: Vec<&'static str>,
}

impl CompressionStats {
    pub fn saved(&self) -> usize {
        self.bytes_before.saturating_sub(self.bytes_after)
    }
    /// Fraction of compressed-blob bytes saved (0.0–1.0).
    pub fn ratio(&self) -> f64 {
        if self.bytes_before == 0 {
            0.0
        } else {
            self.saved() as f64 / self.bytes_before as f64
        }
    }
}

/// Compress tool-result text in `req` in place. Returns byte-measured stats.
/// Only `ContentPart::ToolResult` content is ever touched; error results and
/// out-of-range sizes are skipped; every transform is fail-safe (see module
/// docs). Prompts, system text, and tool *calls* are never modified.
pub fn compress_request(req: &mut AiRequest) -> CompressionStats {
    let mut stats = CompressionStats::default();
    for message in &mut req.messages {
        for part in &mut message.content {
            if let ContentPart::ToolResult {
                content, is_error, ..
            } = part
            {
                if *is_error {
                    continue; // high-signal; never compressed
                }
                let before = content.len();
                if !(MIN_COMPRESS_BYTES..=MAX_COMPRESS_BYTES).contains(&before) {
                    continue;
                }
                let (compressed, filter) = compress_text(content);
                stats.bytes_before += before;
                stats.bytes_after += compressed.len();
                if let Some(name) = filter {
                    stats.filters_applied.push(name);
                }
                *content = compressed;
            }
        }
    }
    stats
}

/// Compress one blob. Returns `(text, Some(filter))` if a filter shrank it, or
/// `(original, None)` if nothing matched / it didn't help / a filter panicked.
pub fn compress_text(text: &str) -> (String, Option<&'static str>) {
    if let Some((name, filter)) = autodetect(text) {
        if let Some(out) = safe_apply(text, filter) {
            return (out, Some(name));
        }
    }
    (text.to_string(), None)
}

/// Apply a filter under the never-empty / never-grow / catch_unwind guards.
/// `None` means "no benefit, keep the original".
fn safe_apply<F: Fn(&str) -> String>(text: &str, filter: F) -> Option<String> {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| filter(text)));
    match result {
        Ok(out) if !out.is_empty() && out.len() < text.len() => Some(out),
        // panicked, grew, or emptied -> passthrough
        _ => None,
    }
}

type Filter = fn(&str) -> String;

/// Pick a filter by sniffing the head of the blob. Ordered cascade; the first
/// match wins. Build-output is checked before grep/find to avoid mis-detecting
/// compiler progress lines.
fn autodetect(text: &str) -> Option<(&'static str, Filter)> {
    let head: Vec<&str> = text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .take(8)
        .collect();
    if head.is_empty() {
        return None;
    }

    if head
        .iter()
        .any(|l| l.starts_with("diff --git ") || l.starts_with("@@ ") || l.starts_with("--- a/"))
    {
        return Some(("git_diff", filter_git_diff));
    }

    let tree_like = head.iter().filter(|l| is_tree_line(l)).count();
    if tree_like >= 2 {
        return Some(("tree", filter_tree));
    }

    let stack_like = head.iter().filter(|l| is_stack_trace_line(l)).count();
    if stack_like >= 2 || head.iter().any(|l| l.trim() == "stack backtrace:") {
        return Some(("stack_trace", filter_stack_trace));
    }

    if head.iter().any(|l| {
        let t = l.trim_start();
        t.starts_with("Compiling ")
            || t.starts_with("error[")
            || t.starts_with("error:")
            || t.starts_with("warning:")
            || t.starts_with("Finished ")
    }) {
        return Some(("build_output", filter_build_output));
    }

    let grep_like = head.iter().filter(|l| parse_grep_line(l).is_some()).count();
    if grep_like >= 2 && grep_like * 2 >= head.len() {
        return Some(("grep", filter_grep));
    }

    let ls_like = head.iter().filter(|l| is_ls_long_line(l)).count();
    if ls_like >= 2 || (head.first().is_some_and(|l| l.starts_with("total ")) && ls_like >= 1) {
        return Some(("ls", filter_ls));
    }

    let nonempty: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    if nonempty.len() >= 3 && nonempty.iter().all(|l| is_path_like(l)) {
        return Some(("find", filter_find));
    }

    // Last resort: collapse dup lines / head+tail truncate for big generic blobs.
    if nonempty.len() >= 8 {
        return Some(("dedup_truncate", filter_dedup_truncate));
    }

    None
}

fn is_path_like(line: &str) -> bool {
    let t = line.trim();
    !t.is_empty() && !t.contains(char::is_whitespace) && (t.contains('/') || t.contains('.'))
}

fn is_tree_line(line: &str) -> bool {
    let t = line.trim_start();
    t.contains("├──")
        || t.contains("└──")
        || t.contains("│")
        || t.starts_with("|-- ")
        || t.starts_with("`-- ")
}

fn is_ls_long_line(line: &str) -> bool {
    let t = line.trim_start();
    if t.starts_with("total ") {
        return false;
    }
    let Some(first) = t.as_bytes().first().copied() else {
        return false;
    };
    matches!(first as char, '-' | 'd' | 'l')
        && t.len() > 10
        && t.as_bytes()
            .get(10)
            .is_some_and(|b| b.is_ascii_whitespace())
        && t.split_whitespace().count() >= 8
}

fn is_stack_trace_line(line: &str) -> bool {
    let t = line.trim_start();
    t.starts_with("at ")
        || t.starts_with("at\t")
        || t.starts_with("at async ")
        || t.starts_with("0:")
        || t.starts_with("1:")
        || t.contains(" panicked at ")
        || (t.contains(".rs:") && t.contains("::"))
}

/// `file:line:content` grep line -> (file, "line: content").
fn parse_grep_line(line: &str) -> Option<(String, String)> {
    let (file, rest) = line.split_once(':')?;
    let (num, content) = rest.split_once(':')?;
    if file.is_empty() || num.is_empty() || !num.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    Some((file.to_string(), format!("{num}: {}", content.trim())))
}

fn filter_git_diff(text: &str) -> String {
    let mut files: Vec<(String, usize, usize)> = Vec::new();
    let mut cur: Option<(String, usize, usize)> = None;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            if let Some(done) = cur.take() {
                files.push(done);
            }
            let path = rest
                .split_whitespace()
                .next()
                .unwrap_or("?")
                .trim_start_matches("a/")
                .to_string();
            cur = Some((path, 0, 0));
        } else if line.starts_with("+++")
            || line.starts_with("---")
            || line.starts_with("@@")
            || line.starts_with("index ")
        {
            // file / hunk headers — never counted as content
        } else if let Some((_, added, removed)) = cur.as_mut() {
            if line.starts_with('+') {
                *added += 1;
            } else if line.starts_with('-') {
                *removed += 1;
            }
        }
    }
    if let Some(done) = cur.take() {
        files.push(done);
    }
    if files.is_empty() {
        return text.to_string();
    }
    let mut out = String::from("[git diff summary]");
    for (path, added, removed) in files {
        out.push_str(&format!("\n  {path}: +{added} -{removed}"));
    }
    out
}

fn filter_grep(text: &str) -> String {
    use std::collections::BTreeMap;
    let mut by_file: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for line in text.lines() {
        if let Some((file, rest)) = parse_grep_line(line) {
            by_file.entry(file).or_default().push(rest);
        }
    }
    if by_file.is_empty() {
        return text.to_string();
    }
    let mut out = String::from("[grep matches by file]");
    for (file, matches) in &by_file {
        out.push_str(&format!("\n{file} ({} matches)", matches.len()));
        for m in matches.iter().take(10) {
            out.push_str(&format!("\n  {m}"));
        }
        if matches.len() > 10 {
            out.push_str(&format!("\n  … {} more", matches.len() - 10));
        }
    }
    out
}

fn filter_tree(text: &str) -> String {
    use std::collections::BTreeMap;

    let mut top: BTreeMap<String, usize> = BTreeMap::new();
    let mut files = 0usize;
    let mut dirs = 0usize;

    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed == "." {
            continue;
        }
        let entry = trimmed
            .trim_start_matches('│')
            .trim_start()
            .trim_start_matches("├──")
            .trim_start_matches("└──")
            .trim_start_matches("|--")
            .trim_start_matches("`--")
            .trim();
        if entry.is_empty() {
            continue;
        }
        if entry.contains('.') {
            files += 1;
        } else {
            dirs += 1;
        }
        let root = entry.split('/').next().unwrap_or(entry).to_string();
        *top.entry(root).or_insert(0) += 1;
    }

    let mut out = format!("[tree summary]\n  {files} files, {dirs} dirs");
    for (name, count) in top.iter().take(20) {
        out.push_str(&format!("\n  {name}: {count} entries"));
    }
    if top.len() > 20 {
        out.push_str(&format!("\n  … {} more entries", top.len() - 20));
    }
    out
}

fn filter_ls(text: &str) -> String {
    let mut files = 0usize;
    let mut dirs = 0usize;
    let mut links = 0usize;
    let mut names = Vec::new();

    for line in text.lines() {
        let t = line.trim_start();
        if !is_ls_long_line(t) {
            continue;
        }
        match t.as_bytes().first().map(|b| *b as char) {
            Some('d') => dirs += 1,
            Some('l') => links += 1,
            _ => files += 1,
        }
        if let Some(name) = t.split_whitespace().last() {
            names.push(name.to_string());
        }
    }

    let total = files + dirs + links;
    let mut out =
        format!("[ls summary]\n  {total} entries: {files} files, {dirs} dirs, {links} links");
    for name in names.iter().take(20) {
        out.push_str(&format!("\n  {name}"));
    }
    if names.len() > 20 {
        out.push_str(&format!("\n  … {} more", names.len() - 20));
    }
    out
}

fn filter_stack_trace(text: &str) -> String {
    let mut frames = Vec::new();
    let mut header = None;

    for line in text.lines() {
        let t = line.trim();
        if header.is_none() && !t.is_empty() && !is_stack_trace_line(t) {
            header = Some(t.to_string());
            continue;
        }
        if is_stack_trace_line(t) {
            frames.push(compact_stack_frame(t));
        }
    }

    let mut out = String::from("[stack trace summary]");
    if let Some(header) = header {
        out.push_str(&format!("\n  {header}"));
    }
    out.push_str(&format!("\n  {} frames", frames.len()));
    for frame in frames.iter().take(20) {
        out.push_str(&format!("\n  {frame}"));
    }
    if frames.len() > 20 {
        out.push_str(&format!("\n  … {} more frames", frames.len() - 20));
    }
    out
}

fn compact_stack_frame(frame: &str) -> String {
    let trimmed = frame.trim_start_matches("at ").trim();
    if let Some(start) = trimmed.rfind('(') {
        let location = trimmed[start + 1..].trim_end_matches(')');
        return location.to_string();
    }
    trimmed.to_string()
}

fn filter_find(text: &str) -> String {
    use std::collections::BTreeMap;
    let mut by_dir: BTreeMap<String, usize> = BTreeMap::new();
    for line in text.lines() {
        let path = line.trim();
        if path.is_empty() {
            continue;
        }
        let dir = path
            .rsplit_once('/')
            .map(|(d, _)| d.to_string())
            .unwrap_or_else(|| ".".to_string());
        *by_dir.entry(dir).or_insert(0) += 1;
    }
    if by_dir.is_empty() {
        return text.to_string();
    }
    let mut out = String::from("[paths grouped by directory]");
    for (dir, count) in by_dir.iter().take(40) {
        out.push_str(&format!("\n  {dir}/ — {count} entries"));
    }
    if by_dir.len() > 40 {
        out.push_str(&format!("\n  … {} more dirs", by_dir.len() - 40));
    }
    out
}

fn filter_build_output(text: &str) -> String {
    let mut progress = 0usize;
    let mut errors = Vec::new();
    let mut warnings = Vec::new();
    for line in text.lines() {
        let t = line.trim_start();
        if t.starts_with("Compiling ")
            || t.starts_with("Downloading ")
            || t.starts_with("Downloaded ")
            || t.starts_with("Checking ")
        {
            progress += 1;
        } else if t.starts_with("error") {
            errors.push(line.to_string());
        } else if t.starts_with("warning") {
            warnings.push(line.to_string());
        }
    }
    let mut out = String::from("[build output summary]");
    if progress > 0 {
        out.push_str(&format!("\n  {progress} progress lines collapsed"));
    }
    for e in errors.iter().take(20) {
        out.push('\n');
        out.push_str(e);
    }
    if errors.len() > 20 {
        out.push_str(&format!("\n  … {} more errors", errors.len() - 20));
    }
    for w in warnings.iter().take(5) {
        out.push('\n');
        out.push_str(w);
    }
    if warnings.len() > 5 {
        out.push_str(&format!("\n  … {} more warnings", warnings.len() - 5));
    }
    out
}

fn filter_dedup_truncate(text: &str) -> String {
    // Collapse consecutive duplicate lines.
    let mut collapsed: Vec<(String, usize)> = Vec::new();
    for line in text.lines() {
        match collapsed.last_mut() {
            Some(last) if last.0 == line => last.1 += 1,
            _ => collapsed.push((line.to_string(), 1)),
        }
    }
    let mut lines: Vec<String> = collapsed
        .into_iter()
        .map(|(line, count)| {
            if count > 1 {
                format!("{line}  (×{count})")
            } else {
                line
            }
        })
        .collect();

    // Head + tail if still long.
    const HEAD: usize = 120;
    const TAIL: usize = 60;
    if lines.len() > HEAD + TAIL + 1 {
        let omitted = lines.len() - HEAD - TAIL;
        let mut out: Vec<String> = lines[..HEAD].to_vec();
        out.push(format!("  … {omitted} lines omitted …"));
        out.extend_from_slice(&lines[lines.len() - TAIL..]);
        lines = out;
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use sb_core::{Message, Role};

    fn big(s: &str, reps: usize) -> String {
        s.repeat(reps)
    }

    #[test]
    fn never_grows_on_random_prose() {
        let prose = big("the quick brown fox jumps over the lazy dog. ", 40);
        assert!(prose.len() >= MIN_COMPRESS_BYTES);
        let (out, _) = compress_text(&prose);
        assert!(out.len() <= prose.len(), "compression grew the input");
    }

    #[test]
    fn safe_apply_passes_through_a_panicking_filter() {
        let input = big("x\n", 300);
        let out = safe_apply(&input, |_| panic!("boom"));
        assert!(out.is_none(), "a panicking filter must degrade to no-op");
    }

    #[test]
    fn safe_apply_rejects_growth_and_empty() {
        let input = "small".to_string();
        assert!(safe_apply(&input, |t| format!("{t} and more")).is_none());
        assert!(safe_apply(&input, |_| String::new()).is_none());
        assert_eq!(
            safe_apply(&input, |_| "ok".to_string()),
            Some("ok".to_string())
        );
    }

    #[test]
    fn git_diff_is_summarized() {
        let diff = format!(
            "diff --git a/src/main.rs b/src/main.rs\n\
             index 111..222 100644\n\
             --- a/src/main.rs\n\
             +++ b/src/main.rs\n\
             @@ -1,3 +1,3 @@\n\
             {}{}",
            big("+added line\n", 60),
            big("-removed line\n", 20),
        );
        assert!(diff.len() >= MIN_COMPRESS_BYTES);
        let (out, filter) = compress_text(&diff);
        assert_eq!(filter, Some("git_diff"));
        assert!(out.contains("src/main.rs: +60 -20"), "got: {out}");
        assert!(out.len() < diff.len());
    }

    #[test]
    fn grep_is_grouped_by_file() {
        let mut blob = String::new();
        for i in 0..40 {
            blob.push_str(&format!("src/lib.rs:{i}:    let x = {i};\n"));
        }
        for i in 0..30 {
            blob.push_str(&format!("src/main.rs:{i}:    foo({i});\n"));
        }
        let (out, filter) = compress_text(&blob);
        assert_eq!(filter, Some("grep"));
        assert!(out.contains("src/lib.rs (40 matches)"), "got: {out}");
        assert!(
            out.contains("… 30 more") || out.contains("… 20 more"),
            "got: {out}"
        );
        assert!(out.len() < blob.len());
    }

    #[test]
    fn tree_output_is_summarized() {
        let mut blob = String::from(".\n");
        for crate_id in 0..12 {
            blob.push_str(&format!("├── crate-{crate_id}\n"));
            for file_id in 0..20 {
                blob.push_str(&format!("│   ├── src/file_{file_id}.rs\n"));
            }
        }

        let (out, filter) = compress_text(&blob);
        assert_eq!(filter, Some("tree"));
        assert!(out.contains("[tree summary]"), "got: {out}");
        assert!(out.contains("crate-0"), "got: {out}");
        assert!(out.len() < blob.len());
    }

    #[test]
    fn ls_long_output_is_summarized() {
        let mut blob = String::from("total 1000\n");
        for i in 0..80 {
            blob.push_str(&format!(
                "-rw-r--r--  1 umut  staff  {:>5} May 31 12:{:02} file_{i}.rs\n",
                100 + i,
                i % 60
            ));
        }

        let (out, filter) = compress_text(&blob);
        assert_eq!(filter, Some("ls"));
        assert!(out.contains("[ls summary]"), "got: {out}");
        assert!(out.contains("80 entries"), "got: {out}");
        assert!(out.len() < blob.len());
    }

    #[test]
    fn stack_trace_is_summarized() {
        let mut blob = String::from("Error: boom\n");
        for i in 0..90 {
            blob.push_str(&format!(
                "    at module_{i}.run (/workspace/project/src/file_{i}.ts:{}:13)\n",
                i + 1
            ));
        }

        let (out, filter) = compress_text(&blob);
        assert_eq!(filter, Some("stack_trace"));
        assert!(out.contains("[stack trace summary]"), "got: {out}");
        assert!(out.contains("file_0.ts:1"), "got: {out}");
        assert!(out.len() < blob.len());
    }

    #[test]
    fn compress_request_skips_error_results_and_keeps_calls() {
        let big_diff = format!("diff --git a/x b/x\n{}", big("+line\n", 200));
        let mut req = AiRequest::new(
            "m",
            vec![Message {
                role: Role::Tool,
                content: vec![
                    ContentPart::ToolResult {
                        tool_use_id: "ok".into(),
                        content: big_diff.clone(),
                        is_error: false,
                    },
                    ContentPart::ToolResult {
                        tool_use_id: "err".into(),
                        content: big_diff.clone(),
                        is_error: true, // must NOT be compressed
                    },
                ],
            }],
        );

        let stats = compress_request(&mut req);
        assert!(
            stats.saved() > 0,
            "expected savings on the non-error result"
        );
        let parts = &req.messages[0].content;
        // non-error result compressed
        if let ContentPart::ToolResult { content, .. } = &parts[0] {
            assert!(content.contains("git diff summary"));
        } else {
            panic!("expected tool result");
        }
        // error result untouched (still the raw diff)
        if let ContentPart::ToolResult { content, .. } = &parts[1] {
            assert_eq!(*content, big_diff);
        } else {
            panic!("expected tool result");
        }
    }

    #[test]
    fn under_min_size_is_untouched() {
        let small = "src/x.rs:1:tiny".to_string();
        assert!(small.len() < MIN_COMPRESS_BYTES);
        let mut req = AiRequest::new(
            "m",
            vec![Message {
                role: Role::Tool,
                content: vec![ContentPart::ToolResult {
                    tool_use_id: "t".into(),
                    content: small.clone(),
                    is_error: false,
                }],
            }],
        );
        let stats = compress_request(&mut req);
        assert_eq!(
            stats.bytes_before, 0,
            "below-threshold blob must be skipped"
        );
        if let ContentPart::ToolResult { content, .. } = &req.messages[0].content[0] {
            assert_eq!(*content, small);
        }
    }
}
