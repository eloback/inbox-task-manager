//! Fuzzy matching for the project finder and folder completion (R1.6): the
//! letters of a query in order, in any case. Linear in the text, so long
//! names with repeated letters never stall the UI (DESIGN.md section 4).

/// A match's score (higher is better) and the positions (in characters) of
/// the letters it matched.
pub type Match = (u32, Vec<usize>);

/// `query`'s letters in order in `text`, ignoring case; None when absent. A
/// prefix ranks first, then a substring at a word start, then any
/// substring, then scattered letters (word starts and runs help, gaps cost).
pub fn fuzzy_match(query: &str, text: &str) -> Option<Match> {
    let original: Vec<char> = text.chars().collect();
    // A letter whose lower case is longer (İ) would shift positions: compare exactly then.
    let lowered =
        |s: &str| -> Option<Vec<char>> { s.chars().map(|c| one_char(c.to_lowercase())).collect::<Option<Vec<char>>>() };
    let (q, t) = match (lowered(query), lowered(text)) {
        (Some(q), Some(t)) => (q, t),
        _ => (query.chars().collect(), original.clone()),
    };
    if q.is_empty() {
        return Some((0, vec![]));
    }
    let starts = |i: usize| {
        i == 0 || !original[i - 1].is_alphanumeric() || (original[i - 1].is_lowercase() && original[i].is_uppercase())
    };
    if let Some(at) = t.windows(q.len()).position(|w| w == q.as_slice()) {
        let base = if at == 0 {
            3000
        } else if starts(at) {
            2000
        } else {
            1000
        };
        return Some((base - at.min(999) as u32, (at..at + q.len()).collect()));
    }
    let mut positions = Vec::with_capacity(q.len());
    let mut from = 0;
    for c in &q {
        let at = from + t[from..].iter().position(|x| x == c)?;
        positions.push(at);
        from = at + 1;
    }
    let hits = positions.iter().filter(|&&p| starts(p)).count() as i64;
    let runs = positions.windows(2).filter(|w| w[1] == w[0] + 1).count() as i64;
    let gaps = (positions[positions.len() - 1] - positions[0] + 1 - positions.len()) as i64;
    Some(((500 + 20 * hits + 10 * runs - gaps).clamp(1, 999) as u32, positions))
}

fn one_char(mut chars: impl Iterator<Item = char>) -> Option<char> {
    let first = chars.next()?;
    chars.next().is_none().then_some(first)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// R1.6: after Python's `test_fuzzy_match`.
    #[test]
    fn r1_6_letters_in_order_any_case_prefixes_first() {
        assert_eq!(fuzzy_match("api", "api").unwrap().1, [0, 1, 2]);
        let score = |q: &str, t: &str| fuzzy_match(q, t).unwrap().0;
        let (prefix, word, inner, letters) =
            (score("ba", "backend"), score("api", "backend-api"), score("ken", "backend"), score("bkd", "backend"));
        assert!(prefix > word && word > inner && inner > letters, "{prefix} {word} {inner} {letters}");
        assert_eq!(fuzzy_match("fl", "fixLogin").unwrap().1, [0, 3], "word starts, camelCase too");
        assert_eq!(fuzzy_match("BAC", "backend").unwrap().1, [0, 1, 2]);
        assert_eq!(fuzzy_match("xyz", "backend"), None);
        assert_eq!(fuzzy_match("aaaaaaaaz", &"a".repeat(400)), None, "linear: no stall on long names");
        assert_eq!(fuzzy_match("", "x"), Some((0, vec![])));
        assert_eq!(fuzzy_match("ü", "Über").unwrap().1, [0]);
        assert_eq!(fuzzy_match("i", "İx").map(|m| m.1), None, "İ lowers to two characters: exact case");
    }
}
