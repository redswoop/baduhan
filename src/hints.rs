//! Quick-select hints (kitty-style): scan visible rows for "interesting"
//! tokens — URLs, paths, hashes — and label them for keyboard pickup.

#[derive(Clone, Debug, PartialEq)]
pub struct HintMatch {
    pub label: String,
    pub text: String,
    /// Viewport position of the token start.
    pub row: usize,
    pub col: usize,
}

/// Is this whitespace-delimited token worth labeling?
pub fn interesting(token: &str) -> bool {
    let t = token.trim_matches(['"', '\'', '(', ')', '[', ']', '<', '>', ',', ';']);
    if t.len() < 5 {
        return false;
    }
    // URL
    if t.contains("://") || t.starts_with("www.") {
        return true;
    }
    // Git-ish hash: 7+ hex chars, all hex.
    if t.len() >= 7 && t.len() <= 64 && t.bytes().all(|b| b.is_ascii_hexdigit()) {
        return true;
    }
    // Windows path (C:\… or C:/…), UNC, or unix-ish path with a separator.
    let b = t.as_bytes();
    if b.len() > 3 && b[1] == b':' && (b[2] == b'\\' || b[2] == b'/') && b[0].is_ascii_alphabetic()
    {
        return true;
    }
    if t.starts_with("\\\\") || (t.starts_with('/') && t.len() > 2) {
        return true;
    }
    // Relative path with at least one separator and an extension-ish tail.
    if (t.contains('/') || t.contains('\\')) && t.rsplit(['/', '\\']).next().is_some_and(|f| f.contains('.')) {
        return true;
    }
    false
}

/// Trim the punctuation that often clings to tokens in prose/log output.
pub fn clean(token: &str) -> &str {
    token
        .trim_start_matches(['"', '\'', '(', '[', '<'])
        .trim_end_matches(['"', '\'', ')', ']', '>', '.', ',', ';', ':'])
}

/// Label generator: home row first, then the rest; two-char labels beyond.
pub fn labels(n: usize) -> Vec<String> {
    const ORDER: &[u8] = b"asdfjkl;ghqwertyuiopzxcvbnm";
    let mut out = Vec::with_capacity(n);
    for i in 0..n.min(ORDER.len()) {
        out.push((ORDER[i] as char).to_string());
    }
    let mut i = ORDER.len();
    'outer: for a in ORDER {
        for b in ORDER {
            if out.len() >= n {
                break 'outer;
            }
            let _ = i;
            i += 1;
            out.push(format!("{}{}", *a as char, *b as char));
        }
    }
    out.truncate(n);
    out
}

/// Scan viewport rows (as (text, char-col-of-each-char) pairs) for matches.
pub fn scan(rows: &[(String, Vec<usize>)]) -> Vec<HintMatch> {
    let mut found: Vec<(usize, usize, String)> = Vec::new();
    for (row_idx, (text, cols)) in rows.iter().enumerate() {
        let chars: Vec<char> = text.chars().collect();
        let mut start = None;
        for i in 0..=chars.len() {
            let is_space = i == chars.len() || chars[i].is_whitespace();
            match (start, is_space) {
                (None, false) => start = Some(i),
                (Some(s), true) => {
                    let token: String = chars[s..i].iter().collect();
                    let cleaned = clean(&token);
                    if interesting(cleaned) && !found.iter().any(|(_, _, t)| t == cleaned) {
                        // Column of the cleaned token start.
                        let lead = token.find(cleaned).unwrap_or(0);
                        let char_pos = s + token[..lead].chars().count();
                        let col = cols.get(char_pos).copied().unwrap_or(0);
                        found.push((row_idx, col, cleaned.to_string()));
                    }
                    start = None;
                },
                _ => {},
            }
        }
    }
    found.truncate(60);
    let labels = labels(found.len());
    found
        .into_iter()
        .zip(labels)
        .map(|((row, col, text), label)| HintMatch { label, text, row, col })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interesting_classification() {
        assert!(interesting("https://example.com/x"));
        assert!(interesting("C:\\Users\\anaka\\file.txt"));
        assert!(interesting("C:/work/proj"));
        assert!(interesting("/etc/nginx/nginx.conf"));
        assert!(interesting("src/window.rs"));
        assert!(interesting("d92fdf1aabbcc"));
        assert!(interesting("\\\\wsl$\\Ubuntu\\home"));
        assert!(!interesting("hello"));
        assert!(!interesting("1234"));
        assert!(!interesting("a/b")); // too short
    }

    #[test]
    fn scan_labels_and_positions() {
        let rows = vec![
            ("see https://x.io/a and C:\\tmp\\f.txt".to_string(), (0..40).collect()),
            ("hash d92fdf1 done".to_string(), (0..20).collect()),
        ];
        let m = scan(&rows);
        assert_eq!(m.len(), 3);
        assert_eq!(m[0].text, "https://x.io/a");
        assert_eq!(m[0].row, 0);
        assert_eq!(m[0].col, 4);
        assert_eq!(m[0].label, "a");
        assert_eq!(m[1].text, "C:\\tmp\\f.txt");
        assert_eq!(m[2].text, "d92fdf1");
        assert_eq!(m[2].row, 1);
    }

    #[test]
    fn labels_unique_and_sufficient() {
        let l = labels(40);
        assert_eq!(l.len(), 40);
        let set: std::collections::HashSet<&String> = l.iter().collect();
        assert_eq!(set.len(), 40);
    }
}
