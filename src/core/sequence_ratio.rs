//! A from-scratch port of Python's `difflib.SequenceMatcher(None, a,
//! b).ratio()` (the Ratcliff/Obershelp algorithm), operating character by
//! character exactly as `difflib` does by default.
//!
//! No crate on crates.io implements this specific algorithm (crates like
//! `strsim` offer Jaro-Winkler/Levenshtein instead, which score
//! differently), and `MarketMatcher`'s similarity thresholds (0.5-0.6) were
//! tuned against `difflib`'s specific behavior - so this is hand-rolled for
//! fidelity rather than approximated with a different metric.
//!
//! Note: this intentionally omits `difflib`'s "autojunk" popular-element
//! filtering, which only ever activates for sequences of 200+ elements;
//! market titles are always far shorter, so it never triggers in practice.

use std::collections::HashMap;

pub fn ratio(a: &str, b: &str) -> f64 {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let t = a.len() + b.len();
    if t == 0 {
        return 1.0;
    }

    let mut b2j: HashMap<char, Vec<usize>> = HashMap::new();
    for (j, &ch) in b.iter().enumerate() {
        b2j.entry(ch).or_default().push(j);
    }

    let m = matching_total(&a, &b2j, 0, a.len(), 0, b.len());
    2.0 * m as f64 / t as f64
}

fn find_longest_match(a: &[char], b2j: &HashMap<char, Vec<usize>>, alo: usize, ahi: usize, blo: usize, bhi: usize) -> (usize, usize, usize) {
    let mut besti = alo;
    let mut bestj = blo;
    let mut bestsize = 0usize;
    let mut j2len: HashMap<usize, usize> = HashMap::new();

    for i in alo..ahi {
        let mut newj2len: HashMap<usize, usize> = HashMap::new();
        if let Some(js) = b2j.get(&a[i]) {
            for &j in js {
                if j < blo {
                    continue;
                }
                if j >= bhi {
                    break;
                }
                let k = if j == 0 { 1 } else { j2len.get(&(j - 1)).copied().unwrap_or(0) + 1 };
                newj2len.insert(j, k);
                if k > bestsize {
                    besti = i + 1 - k;
                    bestj = j + 1 - k;
                    bestsize = k;
                }
            }
        }
        j2len = newj2len;
    }

    (besti, bestj, bestsize)
}

fn matching_total(a: &[char], b2j: &HashMap<char, Vec<usize>>, alo: usize, ahi: usize, blo: usize, bhi: usize) -> usize {
    let (i, j, k) = find_longest_match(a, b2j, alo, ahi, blo, bhi);
    if k == 0 {
        return 0;
    }

    let left = if alo < i && blo < j { matching_total(a, b2j, alo, i, blo, j) } else { 0 };
    let right = if i + k < ahi && j + k < bhi { matching_total(a, b2j, i + k, ahi, j + k, bhi) } else { 0 };

    left + k + right
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_strings_have_ratio_one() {
        assert_eq!(ratio("hello world", "hello world"), 1.0);
    }

    #[test]
    fn empty_strings_have_ratio_one() {
        assert_eq!(ratio("", ""), 1.0);
    }

    #[test]
    fn disjoint_strings_have_ratio_zero() {
        assert_eq!(ratio("abc", "xyz"), 0.0);
    }

    #[test]
    fn matches_known_difflib_example() {
        // Python: SequenceMatcher(None, "abc", "abd").ratio() == 0.6666...
        let r = ratio("abc", "abd");
        assert!((r - (2.0 / 3.0)).abs() < 1e-9, "got {r}");
    }
}
