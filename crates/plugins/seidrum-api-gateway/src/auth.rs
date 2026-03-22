//! API key authentication.

/// Validate a provided API key against the expected key.
pub fn validate_api_key(provided: &str, expected: &str) -> bool {
    // Simple constant-length comparison to avoid trivial timing leaks.
    // For a personal platform this is sufficient; use `subtle` crate for
    // production-grade constant-time comparison.
    if provided.len() != expected.len() {
        return false;
    }
    let mut result = 0u8;
    for (a, b) in provided.bytes().zip(expected.bytes()) {
        result |= a ^ b;
    }
    result == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_key() {
        assert!(validate_api_key("secret123", "secret123"));
    }

    #[test]
    fn invalid_key() {
        assert!(!validate_api_key("wrong", "secret123"));
    }

    #[test]
    fn empty_key() {
        assert!(!validate_api_key("", "secret123"));
    }

    #[test]
    fn both_empty() {
        assert!(validate_api_key("", ""));
    }
}
