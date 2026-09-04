//! "did you mean" support. `enabeld = true` must be an error
//! that says `did you mean `enabled`?`, not a silently ignored key. This single
//! decision eliminates the most common failure mode of declarative TOML systems.

/// Levenshtein distance, iterative with a single row.
fn distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    if a.is_empty() {
        return b.len();
    }
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0usize; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        cur[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            cur[j + 1] = (prev[j + 1] + 1).min(cur[j] + 1).min(prev[j] + cost);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

/// The nearest candidate, if one is near enough to be worth suggesting.
/// The threshold scales with length so that `on` does not "mean" `off`.
pub fn nearest<'a>(input: &str, candidates: impl IntoIterator<Item = &'a str>) -> Option<&'a str> {
    let limit = match input.chars().count() {
        0..=3 => 1,
        4..=7 => 2,
        _ => 3,
    };
    candidates
        .into_iter()
        .map(|c| (distance(input, c), c))
        .filter(|(d, _)| *d <= limit)
        // Ties are common (`enabeld` is two edits from both `enable` and
        // `enabled`). Break them on closeness in length, then lexicographically
        // so the suggestion never depends on iteration order.
        .min_by_key(|(d, c)| (*d, c.len().abs_diff(input.len()), *c))
        .map(|(_, c)| c)
}

/// `did you mean `enabled`?`, or nothing.
pub fn did_you_mean<'a>(
    input: &str,
    candidates: impl IntoIterator<Item = &'a str>,
) -> Option<String> {
    nearest(input, candidates).map(|c| format!("did you mean `{c}`?"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEYS: &[&str] = &[
        "enable", "enabled", "disable", "mask", "name", "source", "target",
    ];

    #[test]
    fn suggests_the_obvious_typo() {
        assert_eq!(nearest("enabeld", KEYS.iter().copied()), Some("enabled"));
        assert_eq!(nearest("sorce", KEYS.iter().copied()), Some("source"));
        assert_eq!(nearest("targett", KEYS.iter().copied()), Some("target"));
    }

    #[test]
    fn stays_quiet_when_nothing_is_close() {
        assert_eq!(nearest("kernel", KEYS.iter().copied()), None);
        assert_eq!(nearest("packages", KEYS.iter().copied()), None);
    }

    #[test]
    fn short_inputs_get_a_tight_threshold() {
        // "on" and "off" are one edit apart in the wrong direction; a suggestion
        // here would be worse than none.
        assert_eq!(nearest("on", ["off", "mask"].into_iter()), None);
    }
}
