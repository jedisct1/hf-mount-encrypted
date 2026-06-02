//! Percent-encoding for URL construction.
//!
//! Encrypted names are filesystem-safe but contain URL-reserved characters
//! (`?`, `#`, `%`, ...). The Hub's GET-style endpoints interpolate paths
//! straight into a URL, so each segment is escaped here while the `/`
//! separators are preserved. Paths carried in JSON request bodies are left raw.

const HEX: &[u8; 16] = b"0123456789ABCDEF";

/// Percent-encode every path segment, leaving `/` separators intact. Everything
/// outside the URI unreserved set (`A-Za-z0-9` and `-._~`) is escaped.
pub fn encode_path_segments(path: &str) -> String {
    let mut out = String::with_capacity(path.len() * 3);
    for (i, segment) in path.split('/').enumerate() {
        if i > 0 {
            out.push('/');
        }
        for &b in segment.as_bytes() {
            if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
                out.push(b as char);
            } else {
                out.push('%');
                out.push(HEX[(b >> 4) as usize] as char);
                out.push(HEX[(b & 0x0f) as usize] as char);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_reserved_keeps_separators() {
        assert_eq!(encode_path_segments("a?b#c%d"), "a%3Fb%23c%25d");
        assert_eq!(encode_path_segments("dir/na me"), "dir/na%20me");
        assert_eq!(encode_path_segments("a/b/c"), "a/b/c");
    }

    #[test]
    fn leaves_unreserved_untouched() {
        assert_eq!(encode_path_segments("AZaz09-._~"), "AZaz09-._~");
    }
}
