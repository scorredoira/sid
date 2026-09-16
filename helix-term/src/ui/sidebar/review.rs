//! Convert Git's patch protocol into code and explicit presentation metadata.
use std::path::PathBuf;

use helix_core::Rope;
use helix_view::review::{LineKind, Review, ReviewLine, ReviewSource, Side};

use super::git::Answer;

pub struct ParsedReview {
    pub text: String,
    pub review: Review,
}

impl ParsedReview {
    /// Keep commit prose out of the patch parser and the historical code sources.
    pub fn prepend_commit(&mut self, text: &str) {
        let mut lines = Vec::new();
        let mut introduction = Review::default();
        push(&mut lines, &mut introduction, "Commit", LineKind::Header);
        for line in text.split_terminator('\n') {
            push(&mut lines, &mut introduction, line, LineKind::Context);
        }
        push(&mut lines, &mut introduction, "", LineKind::Separator);
        push(&mut lines, &mut introduction, "Diffs", LineKind::Header);
        self.text.insert_str(0, &(lines.join("\n") + "\n"));
        introduction.lines.append(&mut self.review.lines);
        self.review.lines = introduction.lines;
        self.review.prepare_line_numbers(&self.text);
    }

    /// The diff as two buffers of the same rows, the old side and the new: what the rows
    /// before and after a change say goes on both, a removed line only on the old, an added
    /// one only on the new, and the lines of a change pair up row by row, the shorter side
    /// padded with blank rows so each row faces its counterpart.
    pub fn split(self) -> [ParsedReview; 2] {
        let texts: Vec<&str> = self.text.split_terminator('\n').collect();
        let mut sides = [Side::Old, Side::New].map(|side| {
            let review = Review {
                sources: self
                    .review
                    .sources
                    .iter()
                    .map(|source| ReviewSource {
                        path: source.path.clone(),
                        text: source.text.clone(),
                        syntax: None,
                    })
                    .collect(),
                digits: self.review.digits,
                side: Some(side),
                ..Review::default()
            };
            (Vec::new(), review)
        });
        let lines = &self.review.lines;
        let mut row = 0;
        while row < lines.len() {
            let changed =
                |line: &ReviewLine| matches!(line.kind, LineKind::Added | LineKind::Removed);
            if !changed(&lines[row]) {
                for (texts_of_side, review) in &mut sides {
                    texts_of_side.push(texts[row]);
                    review.lines.push(lines[row].clone());
                }
                row += 1;
                continue;
            }
            let end = row + lines[row..].iter().take_while(|line| changed(line)).count();
            let of_kind = |kind| (row..end).filter(move |&index| lines[index].kind == kind);
            let removed: Vec<usize> = of_kind(LineKind::Removed).collect();
            let added: Vec<usize> = of_kind(LineKind::Added).collect();
            for pair in 0..removed.len().max(added.len()) {
                for ((texts_of_side, review), indexes) in sides.iter_mut().zip([&removed, &added]) {
                    match indexes.get(pair) {
                        Some(&index) => {
                            texts_of_side.push(texts[index]);
                            review.lines.push(lines[index].clone());
                        }
                        None => {
                            texts_of_side.push("");
                            review.lines.push(ReviewLine {
                                kind: LineKind::Separator,
                                old: None,
                                new: None,
                                source: None,
                            });
                        }
                    }
                }
            }
            row = end;
        }
        sides.map(|(texts_of_side, mut review)| {
            let text = texts_of_side.join("\n") + "\n";
            review.prepare_line_numbers(&text);
            ParsedReview { text, review }
        })
    }
}

struct File {
    header: usize,
    old: String,
    new: String,
    status: &'static str,
    sources: [String; 2],
    source_lines: [usize; 2],
    source_index: usize,
    hunks: usize,
}

fn push(lines: &mut Vec<String>, review: &mut Review, text: &str, kind: LineKind) {
    lines.push(text.into());
    review.lines.push(ReviewLine {
        kind,
        old: None,
        new: None,
        source: None,
    });
}

fn finish(file: File, lines: &mut [String], review: &mut Review) {
    let name = if file.old != file.new && file.old != "/dev/null" && file.new != "/dev/null" {
        format!("{} → {}", file.old, file.new)
    } else if file.new == "/dev/null" {
        file.old.clone()
    } else {
        file.new.clone()
    };
    let name: String = name
        .chars()
        .flat_map(|c| {
            if c.is_control() {
                c.escape_default().collect::<Vec<_>>()
            } else {
                vec![c]
            }
        })
        .collect();
    lines[file.header] = format!("{name}{}", file.status);
    let [old, new] = file.sources;
    for (path, text) in [(file.old, old), (file.new, new)] {
        review.sources.push(ReviewSource {
            path: PathBuf::from(path),
            text: Rope::from(text),
            syntax: None,
        });
    }
}

pub fn parse(patch: &str) -> Answer<ParsedReview> {
    let mut review = Review::default();
    let mut lines = Vec::new();
    let mut file: Option<File> = None;
    let mut old = 0usize;
    let mut new = 0usize;
    let mut remaining = [0usize; 2];
    for line in patch.split_terminator('\n') {
        // Consume the hunk by its declared lengths, never by recognizing apparent headers
        // in code (a deleted line can itself begin with "--" or "diff --git").
        if remaining != [0, 0] && !line.starts_with("\\ No newline") {
            let current = file.as_mut().ok_or("git diff: hunk outside a file")?;
            let (kind, sides) = match line.as_bytes().first() {
                Some(b' ') => (LineKind::Context, [true, true]),
                Some(b'+') => (LineKind::Added, [false, true]),
                Some(b'-') => (LineKind::Removed, [true, false]),
                _ => return Err(format!("git diff: incomplete hunk at {line:?}")),
            };
            let code = &line[1..];
            let side = if sides[1] { 1 } else { 0 };
            let source = (current.source_index + side, current.source_lines[side]);
            push(&mut lines, &mut review, code, kind);
            let row = review.lines.last_mut().unwrap();
            row.old = sides[0].then_some(old);
            row.new = sides[1].then_some(new);
            row.source = Some(source);
            for i in 0..2 {
                if sides[i] {
                    remaining[i] = remaining[i]
                        .checked_sub(1)
                        .ok_or("git diff: invalid hunk length")?;
                    current.sources[i].push_str(code);
                    current.sources[i].push('\n');
                    current.source_lines[i] += 1;
                }
            }
            old = old
                .checked_add(usize::from(sides[0]))
                .ok_or("git diff: line number overflow")?;
            new = new
                .checked_add(usize::from(sides[1]))
                .ok_or("git diff: line number overflow")?;
            continue;
        }
        if let Some(paths) = line.strip_prefix("diff --git ") {
            if let Some(current) = file.take() {
                finish(current, &mut lines, &mut review);
            }
            if !lines.is_empty() {
                push(&mut lines, &mut review, "", LineKind::Separator);
            }
            let (old_path, new_path) = header_paths(paths)?;
            file = Some(File {
                header: lines.len(),
                old: old_path,
                new: new_path,
                status: "",
                sources: Default::default(),
                source_lines: [0, 0],
                source_index: review.sources.len(),
                hunks: 0,
            });
            push(&mut lines, &mut review, "", LineKind::Header);
            continue;
        }
        let Some(current) = &mut file else {
            if !line.is_empty() {
                return Err(format!("git diff: unexpected header {line:?}"));
            }
            continue;
        };
        if let Some(range) = line.strip_prefix("@@ ") {
            let mut parts = range.split_whitespace();
            let before = parts.next().ok_or("git diff: missing old range")?;
            let after = parts.next().ok_or("git diff: missing new range")?;
            if parts.next() != Some("@@") {
                return Err("git diff: invalid hunk header".into());
            }
            let (start_old, count_old) = hunk_range(before, '-')?;
            let (start_new, count_new) = hunk_range(after, '+')?;
            old = start_old;
            new = start_new;
            remaining = [count_old, count_new];
            if current.hunks > 0 {
                push(&mut lines, &mut review, "⋯", LineKind::Separator);
                for i in 0..2 {
                    current.sources[i].push('\n');
                    current.source_lines[i] += 1;
                }
            }
            current.hunks += 1;
        } else if let Some(path) = line.strip_prefix("--- ") {
            current.old = patch_path(path, "a/")?;
        } else if let Some(path) = line.strip_prefix("+++ ") {
            current.new = patch_path(path, "b/")?;
        } else if let Some(path) = line
            .strip_prefix("rename from ")
            .or_else(|| line.strip_prefix("copy from "))
        {
            current.old = decode_path(path)?;
        } else if let Some(path) = line
            .strip_prefix("rename to ")
            .or_else(|| line.strip_prefix("copy to "))
        {
            current.new = decode_path(path)?;
        } else if line.starts_with("new file mode ") {
            current.status = "  ·  added";
        } else if line.starts_with("deleted file mode ") {
            current.status = "  ·  deleted";
        } else if let Some(mode) = line.strip_prefix("old mode ") {
            push(
                &mut lines,
                &mut review,
                &format!("Previous permissions: {mode}"),
                LineKind::Note,
            );
        } else if let Some(mode) = line.strip_prefix("new mode ") {
            push(
                &mut lines,
                &mut review,
                &format!("New permissions: {mode}"),
                LineKind::Note,
            );
        } else if line.starts_with("Binary files ") {
            push(
                &mut lines,
                &mut review,
                "Binary file changed",
                LineKind::Note,
            );
        } else if line.starts_with("\\ No newline") {
            push(
                &mut lines,
                &mut review,
                "No newline at end of file",
                LineKind::Note,
            );
        } else if line.starts_with("index ")
            || line.starts_with("similarity index ")
            || line.starts_with("dissimilarity index ")
            || line.is_empty()
        {
            // Object IDs and similarity scores are transport metadata, not code.
        } else {
            return Err(format!("git diff: unsupported patch line {line:?}"));
        }
    }
    if remaining != [0, 0] {
        return Err("git diff: truncated hunk".into());
    }
    if let Some(current) = file {
        finish(current, &mut lines, &mut review);
    }
    if lines.is_empty() {
        push(&mut lines, &mut review, "No file changes", LineKind::Note);
    }
    let largest = review
        .lines
        .iter()
        .flat_map(|line| [line.old, line.new])
        .flatten()
        .max()
        .unwrap_or(1);
    review.digits = largest.to_string().len().max(3);
    let text = lines.join("\n") + "\n";
    review.prepare_line_numbers(&text);
    Ok(ParsedReview { text, review })
}

fn hunk_range(value: &str, prefix: char) -> Answer<(usize, usize)> {
    let value = value
        .strip_prefix(prefix)
        .ok_or("git diff: invalid range prefix")?;
    let (start, count) = value.split_once(',').unwrap_or((value, "1"));
    let start = start.parse().map_err(|_| "git diff: invalid line number")?;
    let count = count.parse().map_err(|_| "git diff: invalid line count")?;
    Ok((start, count))
}

fn patch_path(path: &str, prefix: &str) -> Answer<String> {
    let path = decode_path(path.trim_end_matches('\t'))?;
    Ok(path.strip_prefix(prefix).unwrap_or(&path).to_string())
}

fn header_paths(paths: &str) -> Answer<(String, String)> {
    if paths.starts_with('"') {
        let end = quoted_end(paths)?;
        let old = patch_path(&paths[..end], "a/")?;
        let new = patch_path(paths[end..].trim_start(), "b/")?;
        return Ok((old, new));
    }
    // Git doesn't quote spaces. Prefer the split that names the same path on both
    // sides; renames supply authoritative names in their following metadata lines.
    let candidates: Vec<_> = paths
        .match_indices(" b/")
        .chain(paths.match_indices(" \"b/"))
        .collect();
    let split = candidates
        .iter()
        .find(|(index, _)| {
            paths[..*index].strip_prefix("a/") == paths[*index + 1..].strip_prefix("b/")
        })
        .or_else(|| candidates.first())
        .ok_or("git diff: missing file paths")?
        .0;
    Ok((
        patch_path(&paths[..split], "a/")?,
        patch_path(&paths[split + 1..], "b/")?,
    ))
}

fn quoted_end(value: &str) -> Answer<usize> {
    let mut escaped = false;
    for (index, byte) in value.bytes().enumerate().skip(1) {
        if escaped {
            escaped = false;
        } else if byte == b'\\' {
            escaped = true;
        } else if byte == b'"' {
            return Ok(index + 1);
        }
    }
    Err("git diff: unterminated quoted path".into())
}

fn decode_path(value: &str) -> Answer<String> {
    if !value.starts_with('"') {
        return Ok(value.to_string());
    }
    let end = quoted_end(value)?;
    if end != value.len() {
        return Err("git diff: trailing text after quoted path".into());
    }
    let mut bytes = value.as_bytes()[1..end - 1].iter().copied();
    let mut decoded = Vec::new();
    while let Some(byte) = bytes.next() {
        if byte != b'\\' {
            decoded.push(byte);
            continue;
        }
        let escaped = bytes.next().ok_or("git diff: incomplete path escape")?;
        let byte = match escaped {
            b'"' | b'\\' => escaped,
            b'a' => 7,
            b'b' => 8,
            b't' => b'\t',
            b'n' => b'\n',
            b'v' => 11,
            b'f' => 12,
            b'r' => b'\r',
            b'0'..=b'3' => {
                let second = bytes
                    .next()
                    .filter(|b| (b'0'..=b'7').contains(b))
                    .ok_or("git diff: invalid octal escape")?;
                let third = bytes
                    .next()
                    .filter(|b| (b'0'..=b'7').contains(b))
                    .ok_or("git diff: invalid octal escape")?;
                (escaped - b'0') * 64 + (second - b'0') * 8 + third - b'0'
            }
            _ => return Err("git diff: invalid path escape".into()),
        };
        decoded.push(byte);
    }
    String::from_utf8(decoded).map_err(|err| format!("git diff: invalid UTF-8 path: {err}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_context_shows_the_entire_historical_file_and_preserves_the_review_location() {
        use std::{fs, process::Command};
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let git = |args: &[&str]| {
            let output = Command::new("git")
                .arg("-C")
                .arg(root)
                .args(args)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        git(&["init"]);
        git(&["config", "user.name", "Review test"]);
        git(&["config", "user.email", "review@example.invalid"]);
        git(&["config", "commit.gpgsign", "false"]);
        let before: String = (1..=120).map(|n| format!("line {n}\n")).collect();
        fs::write(root.join("code.txt"), &before).unwrap();
        git(&["add", "."]);
        git(&["commit", "-m", "before"]);
        let after = before.replace("line 60\n", "changed 60\n");
        fs::write(root.join("code.txt"), &after).unwrap();
        git(&["commit", "-am", "after"]);
        // Context must come from the commit, never from the working file.
        fs::write(root.join("code.txt"), "uncommitted work\n").unwrap();
        let excerpt = super::super::git::show(root, "HEAD", &[".".into()], false).unwrap();
        let complete = super::super::git::show(root, "HEAD", &[".".into()], true).unwrap();
        let excerpt = parse(&excerpt).unwrap();
        let complete = parse(&complete).unwrap();
        assert!(!excerpt.text.contains("line 1\n"));
        assert!(complete.text.contains("line 1\n"));
        assert!(complete.text.ends_with("line 120\n"));
        assert!(!complete.text.contains("uncommitted"));
        assert_eq!(complete.review.sources[0].text.to_string(), before);
        assert_eq!(complete.review.sources[1].text.to_string(), after);
        let anchor = excerpt.review.anchor(0).unwrap();
        let row = complete.review.find_anchor(&anchor).unwrap();
        assert_eq!(complete.review.lines[row].old, Some(60));
        assert_eq!(complete.review.lines[row].kind, LineKind::Removed);
        let distant = complete.review.anchor(110).unwrap();
        let back = excerpt.review.find_anchor(&distant).unwrap();
        assert!(matches!(
            excerpt.review.lines[back].kind,
            LineKind::Added | LineKind::Removed
        ));
    }

    #[test]
    fn a_diff_line_blames_to_the_commit_on_its_side() {
        use super::super::git::{blame, show, BlameRequest, BlameText};
        use std::{fs, process::Command};
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let git = |args: &[&str]| {
            let output = Command::new("git")
                .arg("-C")
                .arg(root)
                .args(args)
                .output()
                .unwrap();
            assert!(output.status.success());
            String::from_utf8_lossy(&output.stdout).trim().to_string()
        };
        git(&["init"]);
        git(&["config", "user.name", "Review test"]);
        git(&["config", "user.email", "review@example.invalid"]);
        git(&["config", "commit.gpgsign", "false"]);
        fs::create_dir(root.join("sub")).unwrap();
        fs::write(root.join("sub/code.txt"), "one\ntwo\nthree\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-m", "before"]);
        let before = git(&["rev-parse", "HEAD"]);
        fs::write(root.join("sub/code.txt"), "one\nTWO\nthree\n").unwrap();
        git(&["commit", "-am", "after"]);
        let after = git(&["rev-parse", "HEAD"]);
        let parsed = parse(&show(root, &after, &[".".into()], false).unwrap()).unwrap();
        let review = &parsed.review;
        let row = |kind| review.lines.iter().position(|l| l.kind == kind).unwrap();
        // Asked from a folder below the top, where the diff's paths do not start.
        let ask = |row: usize| {
            let (path, line, old) = review.file_line(row).unwrap();
            let revision = if old {
                format!("{after}^")
            } else {
                after.clone()
            };
            let request = BlameRequest {
                path: path.to_path_buf(),
                line: line - 1,
                text: BlameText::Revision(revision),
            };
            blame(&root.join("sub"), &request)
                .unwrap()
                .commit
                .unwrap()
                .hash
        };
        assert_eq!(ask(row(LineKind::Added)), after);
        assert_eq!(ask(row(LineKind::Removed)), before);
        assert_eq!(review.file_line(0), None);
    }

    #[test]
    fn real_git_root_and_merge_commits_use_the_same_review_format() {
        use std::{fs, process::Command};
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let git = |args: &[&str]| {
            let output = Command::new("git")
                .arg("-C")
                .arg(root)
                .args(args)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        git(&["init", "-b", "main"]);
        git(&["config", "user.name", "Review test"]);
        git(&["config", "user.email", "review@example.invalid"]);
        git(&["config", "commit.gpgsign", "false"]);
        git(&["config", "diff.noprefix", "true"]);
        git(&["config", "diff.context", "0"]);
        fs::write(root.join("café file.rs"), "fn first() {}\n").unwrap();
        fs::write(root.join("binary"), [0, 1, 2]).unwrap();
        git(&["add", "."]);
        git(&[
            "commit",
            "-m",
            "root",
            "-m",
            "Full body: café\n\ndiff --git is prose here.",
        ]);
        git(&["tag", "v1"]);
        let info = super::super::git::commit_text(root, "HEAD").unwrap();
        assert!(info.contains("Autor: Review test <review@example.invalid>"));
        assert!(info.contains("Committer: Review test <review@example.invalid>"));
        assert!(!info.contains("Padre:"));
        assert!(info.contains("Rama: main\nSigue-a: v1\nPrecede-a: v1\n"));
        assert!(info.contains("root\n\nFull body: café\n\ndiff --git is prose here.\n"));
        assert!(info.ends_with("Archivos: 2 · Líneas añadidas: +1 · Líneas eliminadas: −0\n"));
        let patch = super::super::git::show(root, "HEAD", &[".".into()], false).unwrap();
        let parsed = parse(&patch).unwrap();
        assert!(parsed
            .text
            .contains("café file.rs  ·  added\nfn first() {}"));
        assert!(parsed.text.contains("Binary file changed"));
        assert!(!parsed.text.contains("diff --git"));
        git(&["checkout", "-b", "topic"]);
        fs::write(root.join("café file.rs"), "fn second() {}\n").unwrap();
        git(&["commit", "-am", "topic"]);
        git(&["checkout", "main"]);
        fs::write(root.join("other"), "other\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-m", "main"]);
        git(&["merge", "--no-ff", "-m", "merge", "topic"]);
        let patch = super::super::git::show(root, "HEAD", &[".".into()], false).unwrap();
        let parsed = parse(&patch).unwrap();
        assert_eq!(parsed.text, "café file.rs\nfn first() {}\nfn second() {}\n");
        assert_eq!(parsed.review.lines[1].kind, LineKind::Removed);
        assert_eq!(parsed.review.lines[2].kind, LineKind::Added);
        let info = super::super::git::commit_text(root, "HEAD").unwrap();
        assert_eq!(
            info.lines()
                .filter(|line| line.starts_with("Padre:"))
                .count(),
            2
        );
        assert!(info.contains(" (main)\n"));
        assert!(info.contains(" (topic)\n"));
        assert!(info.contains("Sigue-a: v1\nPrecede-a: \n"));
        assert!(info.ends_with("Archivos: 1 · Líneas añadidas: +1 · Líneas eliminadas: −1\n"));
        let anchor = parsed.review.anchor(1).unwrap();
        let mut parsed = parsed;
        parsed.prepend_commit(&info);
        assert!(parsed.text.starts_with("Commit\nAutor:"));
        assert!(parsed.text.contains("\nmerge\n"));
        assert!(parsed.text.contains("\nDiffs\ncafé file.rs\n"));
        let row = parsed.review.find_anchor(&anchor).unwrap();
        assert_eq!(parsed.text.lines().nth(row), Some("fn first() {}"));
        assert!(parsed.review.lines[..row]
            .iter()
            .all(|line| line.source.is_none()));
        git(&["mv", "other", "renamed\tfile\n.txt"]);
        git(&["commit", "-m", "rename"]);
        let info = super::super::git::commit_text(root, "HEAD").unwrap();
        assert!(info.ends_with("Archivos: 1 · Líneas añadidas: +0 · Líneas eliminadas: −0\n"));
        git(&["commit", "--allow-empty", "-m", "empty"]);
        let info = super::super::git::commit_text(root, "HEAD").unwrap();
        assert!(info.ends_with("Archivos: 0 · Líneas añadidas: +0 · Líneas eliminadas: −0\n"));
    }

    #[test]
    fn code_and_original_line_numbers_survive_without_git_metadata() {
        let patch = "diff --git a/demo.rs b/demo.rs\nindex 123..456 100644\n--- a/demo.rs\n+++ b/demo.rs\n@@ -31,3 +31,3 @@ fn demo()\n use one;\n-use old;\n+use new;\n \n@@ -90 +90,2 @@\n-old()\n+new()\n+extra()\n";
        let result = parse(patch).unwrap();
        assert_eq!(
            result.text,
            "demo.rs\nuse one;\nuse old;\nuse new;\n\n⋯\nold()\nnew()\nextra()\n"
        );
        let rows = &result.review.lines;
        assert_eq!((rows[1].old, rows[1].new), (Some(31), Some(31)));
        assert_eq!(
            (rows[2].old, rows[2].new, rows[2].kind),
            (Some(32), None, LineKind::Removed)
        );
        assert_eq!(
            (rows[3].old, rows[3].new, rows[3].kind),
            (None, Some(32), LineKind::Added)
        );
        assert_eq!((rows[8].old, rows[8].new), (None, Some(91)));
        assert_eq!(
            result.review.sources[0].text.to_string(),
            "use one;\nuse old;\n\n\nold()\n"
        );
        assert_eq!(
            result.review.sources[1].text.to_string(),
            "use one;\nuse new;\n\n\nnew()\nextra()\n"
        );
        for (line, row) in rows.iter().enumerate() {
            if let Some((source, source_line)) = row.source {
                let source = result.review.sources[source]
                    .text
                    .line(source_line)
                    .to_string();
                assert_eq!(
                    source.trim_end_matches('\n'),
                    result.text.lines().nth(line).unwrap()
                );
            }
        }
    }

    #[test]
    fn side_by_side_each_row_faces_its_counterpart() {
        let patch = "diff --git a/demo.rs b/demo.rs\n--- a/demo.rs\n+++ b/demo.rs\n@@ -1,7 +1,6 @@\n keep\n-one\n-two\n-three\n+uno\n same\n-gone\n+new\n+newer\n last\n";
        let [old, new] = parse(patch).unwrap().split();
        assert_eq!(
            old.text,
            "demo.rs\nkeep\none\ntwo\nthree\nsame\ngone\n\nlast\n"
        );
        assert_eq!(new.text, "demo.rs\nkeep\nuno\n\n\nsame\nnew\nnewer\nlast\n");
        let kinds = |side: &ParsedReview| -> Vec<LineKind> {
            side.review.lines.iter().map(|line| line.kind).collect()
        };
        use LineKind::*;
        assert_eq!(
            kinds(&old),
            [Header, Context, Removed, Removed, Removed, Context, Removed, Separator, Context]
        );
        assert_eq!(
            kinds(&new),
            [Header, Context, Added, Separator, Separator, Context, Added, Added, Context]
        );
        // Every code row keeps its place in the file, so blame and a switch back to one
        // above the other land on the same line.
        assert_eq!(
            (old.review.lines[6].old, old.review.lines[6].new),
            (Some(6), None)
        );
        assert_eq!(
            (new.review.lines[7].old, new.review.lines[7].new),
            (None, Some(5))
        );
        assert_eq!(
            (old.review.lines[5].old, old.review.lines[5].new),
            (Some(5), Some(3))
        );
        assert_eq!(old.review.lines[7].source, None);
        for side in [&old, &new] {
            for (row, line) in side.review.lines.iter().enumerate() {
                if let Some((source, source_line)) = line.source {
                    let code = side.review.sources[source]
                        .text
                        .line(source_line)
                        .to_string();
                    assert_eq!(
                        code.trim_end_matches('\n'),
                        side.text.lines().nth(row).unwrap()
                    );
                }
            }
        }
    }

    #[test]
    fn side_by_side_numbers_each_side_by_its_own_lines() {
        let patch = "diff --git a/demo.rs b/demo.rs\n--- a/demo.rs\n+++ b/demo.rs\n@@ -10,2 +20,2 @@\n keep\n-old\n+new\n";
        let unified = parse(patch).unwrap();
        let numbers = |review: &Review| -> Vec<Vec<String>> {
            review
                .number_annotations
                .iter()
                .map(|layer| layer.iter().map(|a| a.text.to_string()).collect())
                .collect()
        };
        assert_eq!(numbers(&unified.review)[0], [" 10  20    "]);
        let [old, new] = unified.split();
        assert_eq!(old.review.side, Some(Side::Old));
        assert_eq!(
            numbers(&old.review),
            [vec![" 10    "], vec![], vec![" 11 −  "]]
        );
        assert_eq!(
            numbers(&new.review),
            [vec![" 20    "], vec![" 21 +  "], vec![]]
        );
    }

    #[test]
    fn side_by_side_keeps_the_commit_and_the_notes_on_both_sides() {
        let patch = "diff --git a/a.txt b/a.txt\nnew file mode 100644\n--- /dev/null\n+++ b/a.txt\n@@ -0,0 +1 @@\n+first\n\\ No newline at end of file\n";
        let mut parsed = parse(patch).unwrap();
        parsed.prepend_commit("subject\n");
        let [old, new] = parsed.split();
        assert_eq!(
            old.text,
            "Commit\nsubject\n\nDiffs\na.txt  ·  added\n\nNo newline at end of file\n"
        );
        assert_eq!(
            new.text,
            "Commit\nsubject\n\nDiffs\na.txt  ·  added\nfirst\nNo newline at end of file\n"
        );
    }

    #[test]
    fn hunk_code_that_looks_like_metadata_is_still_code() {
        let result = parse("diff --git a/a.txt b/a.txt\n--- a/a.txt\n+++ b/a.txt\n@@ -1,2 +1,2 @@\n--- old heading\n+++ new heading\n diff --git a/fake b/fake\n").unwrap();
        assert_eq!(
            result.text,
            "a.txt\n-- old heading\n++ new heading\ndiff --git a/fake b/fake\n"
        );
        assert_eq!(
            result
                .review
                .lines
                .iter()
                .filter(|line| line.kind == LineKind::Header)
                .count(),
            1
        );
    }

    #[test]
    fn additions_deletions_and_missing_newlines_keep_the_correct_side() {
        let patch = "diff --git a/new.ts b/new.ts\nnew file mode 100644\n--- /dev/null\n+++ b/new.ts\n@@ -0,0 +1 @@\n+const café = 1;\n\\ No newline at end of file\ndiff --git a/old.ts b/old.ts\ndeleted file mode 100644\n--- a/old.ts\n+++ /dev/null\n@@ -1 +0,0 @@\n-old\n\\ No newline at end of file\n";
        let result = parse(patch).unwrap();
        assert!(result
            .text
            .starts_with("new.ts  ·  added\nconst café = 1;\nNo newline"));
        assert!(result.text.contains("old.ts  ·  deleted\nold\nNo newline"));
        assert_eq!(
            (result.review.lines[1].old, result.review.lines[1].new),
            (None, Some(1))
        );
        assert_eq!(
            (result.review.lines[5].old, result.review.lines[5].new),
            (Some(1), None)
        );
        assert_eq!(result.review.sources[1].path, PathBuf::from("new.ts"));
        assert_eq!(result.review.sources[2].path, PathBuf::from("old.ts"));
    }

    #[test]
    fn non_code_changes_are_not_hidden() {
        let patch = "diff --git a/old name b/new name\nsimilarity index 100%\nrename from old name\nrename to new name\ndiff --git a/run b/run\nold mode 100644\nnew mode 100755\ndiff --git a/picture.png b/picture.png\nindex 123..456 100644\nBinary files a/picture.png and b/picture.png differ\n";
        let result = parse(patch).unwrap();
        assert_eq!(result.text, "old name → new name\n\nrun\nPrevious permissions: 100644\nNew permissions: 100755\n\npicture.png\nBinary file changed\n");
    }

    #[test]
    fn quoted_unicode_and_ambiguous_spaces_are_decoded() {
        let result = parse("diff --git \"a/caf\\303\\251\\t.rs\" \"b/caf\\303\\251\\t.rs\"\nnew file mode 100644\n").unwrap();
        assert_eq!(result.text, "café\\t.rs  ·  added\n");
        assert_eq!(result.review.sources[1].path, PathBuf::from("café\t.rs"));
        let result =
            parse("diff --git a/a b/name b/a b/name\nold mode 100644\nnew mode 100755\n").unwrap();
        assert!(result.text.starts_with("a b/name\n"));
    }

    #[test]
    fn malformed_or_incomplete_patches_are_reported() {
        for patch in [
            "diff --git a/a b/a\n@@ -1,2 +1,2 @@\n one\n",
            "diff --git a/a b/a\n@@ -oops +1 @@\n",
            "diff --git a/a b/a\n@@ -1 +1 @@\n+one\n+two\n",
            "diff --git \"a/b b/b\n",
            "diff --git a/a b/a\nunknown transport record\n",
        ] {
            assert!(parse(patch).is_err(), "accepted: {patch}");
        }
        assert_eq!(parse("").unwrap().text, "No file changes\n");
    }
}
