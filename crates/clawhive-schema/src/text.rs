//! Shared text utilities used across channel adapters.

/// Find the largest byte index <= `max` that lies on a UTF-8 char boundary.
pub fn floor_char_boundary(s: &str, max: usize) -> usize {
    if max >= s.len() {
        return s.len();
    }
    let mut i = max;
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_boundary() {
        assert_eq!(floor_char_boundary("hello world", 5), 5);
    }

    #[test]
    fn mid_multibyte() {
        let s = "ab中"; // bytes: 61 62 e4 b8 ad
        assert_eq!(floor_char_boundary(s, 4), 2);
        assert_eq!(floor_char_boundary(s, 3), 2);
        assert_eq!(floor_char_boundary(s, 5), 5);
    }

    #[test]
    fn at_boundary() {
        assert_eq!(floor_char_boundary("abc", 10), 3);
        assert_eq!(floor_char_boundary("", 0), 0);
    }
}
