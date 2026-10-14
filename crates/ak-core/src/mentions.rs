//! `@handle` extraction from markdown bodies.

/// Returns the unique handles mentioned in `text`, lowercased, in order of
/// first appearance. A mention is an `@` at the start of the text or after a
/// non-alphanumeric character, followed by a handle. Email addresses do not
/// count because the `@` follows an alphanumeric character.
pub fn extract_handles(text: &str) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '@' {
            let preceded_ok = i == 0 || !chars[i - 1].is_alphanumeric();
            if preceded_ok {
                let start = i + 1;
                let mut end = start;
                while end < chars.len() && is_handle_char(chars[end]) {
                    end += 1;
                }
                // Trailing punctuation like `@dba.` or `@dba-` is not part of the handle.
                while end > start && matches!(chars[end - 1], '-' | '_') {
                    end -= 1;
                }
                if end - start >= 2 && chars[start].is_ascii_alphabetic() {
                    let handle: String =
                        chars[start..end].iter().collect::<String>().to_lowercase();
                    if !found.contains(&handle) {
                        found.push(handle);
                    }
                }
                i = end.max(i + 1);
                continue;
            }
        }
        i += 1;
    }
    found
}

fn is_handle_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '-' || c == '_'
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_simple_mentions() {
        assert_eq!(extract_handles("ping @backend please"), vec!["backend"]);
    }

    #[test]
    fn dedupes_and_lowercases() {
        assert_eq!(
            extract_handles("@Backend and @backend and @dba"),
            vec!["backend", "dba"]
        );
    }

    #[test]
    fn ignores_emails() {
        assert!(extract_handles("mail me at someone@example.com").is_empty());
    }

    #[test]
    fn strips_trailing_punctuation() {
        assert_eq!(extract_handles("thanks @dba."), vec!["dba"]);
        assert_eq!(extract_handles("(@dba)"), vec!["dba"]);
        assert_eq!(extract_handles("@dba-"), vec!["dba"]);
    }

    #[test]
    fn requires_two_characters() {
        assert!(extract_handles("@a").is_empty());
        assert!(extract_handles("@1x").is_empty());
    }

    #[test]
    fn handles_at_line_starts_and_after_punctuation() {
        assert_eq!(extract_handles("@qa\n@ops,@sec"), vec!["qa", "ops", "sec"]);
    }
}
