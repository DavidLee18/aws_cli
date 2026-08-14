//! A port of Python's `difflib.get_close_matches`, used for `Maybe you meant:`.
//!
//! The suggestions are printed, so the cutoff and the ordering are user-visible: a
//! different similarity measure shows a different list. `argparser.py` calls
//! `get_close_matches(value, choices, cutoff=0.8)`, leaving `n` at its default of 3.
//!
//! `SequenceMatcher.ratio()` is `2*M/T`, where `T` is the combined length and `M` is the
//! total size of the matching blocks found by recursively taking the longest match. Note
//! the argument order: difflib sets the *typed* word as sequence B and each candidate as
//! sequence A, which matters because the longest-match tie-break prefers the earliest
//! block in A.

/// The closest `n` of `candidates` to `word`, best first, above `cutoff`.
pub fn get_close_matches<'a>(
    word: &str,
    candidates: impl IntoIterator<Item = &'a str>,
    n: usize,
    cutoff: f64,
) -> Vec<&'a str> {
    let b: Vec<char> = word.chars().collect();
    let mut scored: Vec<(f64, &'a str)> = Vec::new();
    for candidate in candidates {
        let a: Vec<char> = candidate.chars().collect();
        // difflib's cheap prefilters cannot change the outcome, only the speed, so only
        // the real ratio is computed here.
        let score = ratio(&a, &b);
        if score >= cutoff {
            scored.push((score, candidate));
        }
    }
    // `heapq.nlargest` orders by the whole tuple, so equal scores fall back to the
    // candidate string, descending.
    scored.sort_by(|x, y| {
        y.0.partial_cmp(&x.0).unwrap_or(std::cmp::Ordering::Equal).then_with(|| y.1.cmp(x.1))
    });
    scored.truncate(n);
    scored.into_iter().map(|(_, c)| c).collect()
}

fn ratio(a: &[char], b: &[char]) -> f64 {
    let total = a.len() + b.len();
    if total == 0 {
        return 1.0;
    }
    let matches = matching_total(a, b, 0, a.len(), 0, b.len());
    2.0 * matches as f64 / total as f64
}

/// Sum the matching-block sizes, recursing either side of the longest match.
fn matching_total(a: &[char], b: &[char], alo: usize, ahi: usize, blo: usize, bhi: usize) -> usize {
    let (i, j, size) = longest_match(a, b, alo, ahi, blo, bhi);
    if size == 0 {
        return 0;
    }
    let mut total = size;
    if alo < i && blo < j {
        total += matching_total(a, b, alo, i, blo, j);
    }
    if i + size < ahi && j + size < bhi {
        total += matching_total(a, b, i + size, ahi, j + size, bhi);
    }
    total
}

/// difflib's `find_longest_match`: earliest in A, then earliest in B, then longest.
///
/// `autojunk` only engages for sequences of 200 or more, which command names never are,
/// so no element is ever treated as popular.
fn longest_match(
    a: &[char],
    b: &[char],
    alo: usize,
    ahi: usize,
    blo: usize,
    bhi: usize,
) -> (usize, usize, usize) {
    let (mut best_i, mut best_j, mut best_size) = (alo, blo, 0usize);
    // j2len[j] is the length of the run ending at b[j] for the current a[i].
    let mut j2len: std::collections::HashMap<usize, usize> = std::collections::HashMap::new();

    for (i, ch) in a.iter().enumerate().take(ahi).skip(alo) {
        let mut newj2len: std::collections::HashMap<usize, usize> = std::collections::HashMap::new();
        for (j, other) in b.iter().enumerate().take(bhi).skip(blo) {
            if other != ch {
                continue;
            }
            let k = if j == 0 { 1 } else { j2len.get(&(j - 1)).copied().unwrap_or(0) + 1 };
            newj2len.insert(j, k);
            if k > best_size {
                best_i = i + 1 - k;
                best_j = j + 1 - k;
                best_size = k;
            }
        }
        j2len = newj2len;
    }
    (best_i, best_j, best_size)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Values taken from Python's own difflib, so the printed list matches.
    #[test]
    fn matches_pythons_ratio() {
        let r = |a: &str, b: &str| {
            let av: Vec<char> = a.chars().collect();
            let bv: Vec<char> = b.chars().collect();
            (ratio(&av, &bv) * 1e6).round() / 1e6
        };
        assert_eq!(r("abcd", "abcd"), 1.0);
        assert_eq!(r("", ""), 1.0);
        assert_eq!(r("abcd", "abce"), 0.75);
        // difflib.SequenceMatcher(None, 'copy-option-group', 'modify-option-group').ratio()
        assert_eq!(r("copy-option-group", "modify-option-group"), 0.833333);
        assert_eq!(r("new", "old"), 0.0);
    }

    /// The case the reference actually prints for `rds modify-option-group`.
    #[test]
    fn suggests_what_the_reference_suggests() {
        let choices = [
            "copy-option-group",
            "create-option-group",
            "delete-option-group",
            "describe-option-groups",
            "add-option-to-option-group",
        ];
        assert_eq!(
            get_close_matches("modify-option-group", choices, 3, 0.8),
            ["copy-option-group"]
        );
    }

    /// Nothing similar enough yields nothing, so no `Maybe you meant:` block is printed.
    #[test]
    fn returns_nothing_below_the_cutoff() {
        let choices = ["describe-instances", "run-instances"];
        assert!(get_close_matches("frobnicate", choices, 3, 0.8).is_empty());
    }

    /// At most `n`, best first.
    #[test]
    fn caps_and_orders_results() {
        let choices = ["list-users", "list-user", "list-userz", "list-usery"];
        // Python: ['list-user', 'list-userz', 'list-usery'] -- the shorter candidate
        // scores highest, and equal scores tie-break on the string, descending.
        assert_eq!(get_close_matches("list-usera", choices, 3, 0.8), ["list-user", "list-userz", "list-usery"]);
    }
}
