//! Unified line diffs for edit previews, so agents need not receive both full
//! file texts. Context is three lines, as in `diff -u`.

const CONTEXT: usize = 3;
/// Above this many LCS cells the changed middle is reported as one hunk.
const MAX_CELLS: usize = 4 << 20;

#[derive(Clone, Copy, PartialEq)]
enum Op {
    Keep,
    Delete,
    Insert,
}

/// Line script from `before` to `after`: (op, line index in before or after).
fn script<'a>(before: &[&'a str], after: &[&'a str]) -> Vec<(Op, usize)> {
    let prefix = before.iter().zip(after).take_while(|(a, b)| a == b).count();
    let suffix = before[prefix..]
        .iter()
        .rev()
        .zip(after[prefix..].iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    let old = &before[prefix..before.len() - suffix];
    let new = &after[prefix..after.len() - suffix];
    let mut ops: Vec<(Op, usize)> = (0..prefix).map(|i| (Op::Keep, i)).collect();
    if old.len().saturating_mul(new.len()) > MAX_CELLS {
        ops.extend((0..old.len()).map(|i| (Op::Delete, prefix + i)));
        ops.extend((0..new.len()).map(|i| (Op::Insert, prefix + i)));
    } else {
        // lcs[i][j] = LCS length of old[i..] and new[j..].
        let width = new.len() + 1;
        let mut lcs = vec![0u32; (old.len() + 1) * width];
        for i in (0..old.len()).rev() {
            for j in (0..new.len()).rev() {
                lcs[i * width + j] = if old[i] == new[j] {
                    lcs[(i + 1) * width + j + 1] + 1
                } else {
                    lcs[(i + 1) * width + j].max(lcs[i * width + j + 1])
                };
            }
        }
        let (mut i, mut j) = (0, 0);
        while i < old.len() || j < new.len() {
            if i < old.len() && j < new.len() && old[i] == new[j] {
                ops.push((Op::Keep, prefix + i));
                i += 1;
                j += 1;
            } else if j == new.len()
                || (i < old.len() && lcs[(i + 1) * width + j] >= lcs[i * width + j + 1])
            {
                ops.push((Op::Delete, prefix + i));
                i += 1;
            } else {
                ops.push((Op::Insert, prefix + j));
                j += 1;
            }
        }
    }
    ops.extend((0..suffix).map(|k| (Op::Keep, before.len() - suffix + k)));
    ops
}

fn push_line(out: &mut String, mark: char, line: &str) {
    out.push(mark);
    out.push_str(line);
    if !line.ends_with('\n') {
        out.push_str("\n\\ No newline at end of file\n");
    }
}

/// `diff -u` text for one file; empty when the texts are equal.
pub(super) fn unified(path: &str, before: &str, after: &str) -> String {
    let old: Vec<&str> = before.split_inclusive('\n').collect();
    let new: Vec<&str> = after.split_inclusive('\n').collect();
    let ops = script(&old, &new);
    // Positions (before, after) at the start of each op.
    let mut at = Vec::with_capacity(ops.len() + 1);
    let (mut a, mut b) = (0, 0);
    for &(op, _) in &ops {
        at.push((a, b));
        match op {
            Op::Keep => {
                a += 1;
                b += 1;
            }
            Op::Delete => a += 1,
            Op::Insert => b += 1,
        }
    }
    at.push((a, b));
    let changed: Vec<usize> = (0..ops.len()).filter(|&k| ops[k].0 != Op::Keep).collect();
    let mut out = String::new();
    if changed.is_empty() {
        return out;
    }
    out.push_str(&format!("--- a/{path}\n+++ b/{path}\n"));
    let mut k = 0;
    while k < changed.len() {
        let start = changed[k].saturating_sub(CONTEXT);
        let mut end = changed[k] + 1;
        while k < changed.len() && changed[k] <= end + 2 * CONTEXT {
            end = changed[k] + 1;
            k += 1;
        }
        let end = (end + CONTEXT).min(ops.len());
        let (a0, b0) = at[start];
        let (a1, b1) = at[end];
        let range = |first: usize, count: usize| {
            // Zero-length ranges name the line before the hunk, per diff -u.
            let line = if count == 0 { first } else { first + 1 };
            format!("{line},{count}")
        };
        out.push_str(&format!(
            "@@ -{} +{} @@\n",
            range(a0, a1 - a0),
            range(b0, b1 - b0)
        ));
        for &(op, index) in &ops[start..end] {
            match op {
                Op::Keep => push_line(&mut out, ' ', old[index]),
                Op::Delete => push_line(&mut out, '-', old[index]),
                Op::Insert => push_line(&mut out, '+', new[index]),
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::unified;

    #[test]
    fn unified_diff_perspectives() {
        let ten: String = (1..=10).map(|i| format!("{i}\n")).collect();
        // Equal texts produce nothing.
        assert_eq!(unified("f", &ten, &ten), "");
        assert_eq!(unified("f", "", ""), "");
        // A middle change keeps three context lines on each side.
        let mid = ten.replace("5\n", "five\n");
        assert_eq!(
            unified("f.wi", &ten, &mid),
            "--- a/f.wi\n+++ b/f.wi\n@@ -2,7 +2,7 @@\n 2\n 3\n 4\n-5\n+five\n 6\n 7\n 8\n"
        );
        // Changes at the edges clip context.
        let first = ten.replacen("1\n", "one\n", 1);
        assert!(unified("f", &ten, &first).contains("@@ -1,4 +1,4 @@\n-1\n+one\n 2\n"));
        let last = ten.replace("10\n", "ten\n");
        assert!(unified("f", &ten, &last).ends_with(" 9\n-10\n+ten\n"));
        // Distant changes produce separate hunks; near ones merge.
        let lines: String = (1..=30).map(|i| format!("{i}\n")).collect();
        let far = lines
            .replace("\n3\n", "\nthree\n")
            .replace("\n27\n", "\nx\n");
        assert_eq!(unified("f", &lines, &far).matches("@@ -").count(), 2);
        let near = lines
            .replace("\n3\n", "\nthree\n")
            .replace("\n8\n", "\nx\n");
        assert_eq!(unified("f", &lines, &near).matches("@@ -").count(), 1);
        // Pure insertion and deletion.
        let inserted = ten.replace("5\n", "5\nnew\n");
        assert!(unified("f", &ten, &inserted).contains("@@ -3,6 +3,7 @@\n"));
        let deleted = ten.replace("5\n", "");
        assert!(unified("f", &ten, &deleted).contains("@@ -2,7 +2,6 @@\n"));
        // New and emptied files use zero-length ranges.
        assert_eq!(
            unified("f", "", "a\n"),
            "--- a/f\n+++ b/f\n@@ -0,0 +1,1 @@\n+a\n"
        );
        assert_eq!(
            unified("f", "a\n", ""),
            "--- a/f\n+++ b/f\n@@ -1,1 +0,0 @@\n-a\n"
        );
        // Missing trailing newline is marked.
        let text = unified("f", "a", "b");
        assert_eq!(
            text,
            "--- a/f\n+++ b/f\n@@ -1,1 +1,1 @@\n-a\n\\ No newline at end of file\n+b\n\\ No newline at end of file\n"
        );
        // CRLF line endings are preserved in the hunk body.
        assert!(unified("f", "a\r\nb\r\n", "a\r\nc\r\n").contains("-b\r\n+c\r\n"));
        // Repeated lines still yield a minimal script.
        let diff = unified("f", "x\nx\nx\n", "x\nx\n");
        assert_eq!(
            diff.lines()
                .filter(|l| l.starts_with('-') && *l != "--- a/f")
                .count(),
            1
        );
        // Applying the hunks' +/space lines reproduces the new text.
        let rebuilt: String = unified("f", &ten, &mid)
            .lines()
            .skip(3)
            .filter(|l| !l.starts_with('-'))
            .map(|l| format!("{}\n", &l[1..]))
            .collect();
        assert_eq!(rebuilt, "2\n3\n4\nfive\n6\n7\n8\n");
    }
}
