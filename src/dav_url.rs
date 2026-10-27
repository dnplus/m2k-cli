use reqwest::Url;

/// Normalize collection identity without decoding path separators or URL delimiters.
/// Keep escaped dots encoded so normalization cannot introduce dot segments.
pub(crate) fn collection_id(url: &Url) -> String {
    let mut normalized = url.clone();
    let path = normalized_path(url.path());
    normalized.set_path(&format!("{}/", path.trim_end_matches('/')));
    normalized.to_string()
}

pub(crate) fn identity(value: &str) -> String {
    Url::parse(value.trim())
        .map(|url| collection_id(&url))
        .unwrap_or_else(|_| value.trim().to_owned())
}

pub(crate) fn normalized_path(path: &str) -> String {
    let mut result = String::with_capacity(path.len());
    let mut chars = path.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '%' {
            let mut probe = chars.clone();
            if let (Some(high), Some(low)) = (probe.next(), probe.next()) {
                if let (Some(high), Some(low)) = (high.to_digit(16), low.to_digit(16)) {
                    chars = probe;
                    let byte = (high * 16 + low) as u8;
                    if byte.is_ascii_alphanumeric() || b"-_~@".contains(&byte) {
                        result.push(byte as char);
                    } else {
                        result.push_str(&format!("%{byte:02X}"));
                    }
                    continue;
                }
            }
        }
        result.push(c);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn equivalent_collections_and_safe_path_boundaries() {
        assert_eq!(
            identity("https://example.com/u%40example.com/%7eabc"),
            identity("https://example.com/u@example.com/~abc/")
        );
        assert_eq!(
            identity("https://example.com/%7Eabc/"),
            identity("https://example.com/~abc")
        );
        for (encoded, literal) in [
            ("a%2fb", "a/b"),
            ("a%3fb", "a?b"),
            ("a%23b", "a#b"),
            ("a%2540b", "a@b"),
        ] {
            assert_ne!(
                identity(&format!("https://example.com/{encoded}")),
                identity(&format!("https://example.com/{literal}"))
            );
        }
        assert_ne!(
            identity("https://example.com/A"),
            identity("https://example.com/a")
        );
        assert_ne!(
            identity("https://example.com/a?q=1"),
            identity("https://example.com/a?q=2")
        );
    }
}
