use anyhow::Result;
use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::Read;
use std::path::Path;
use tracing::info;

/// Verify SHA-256 checksum of a file
pub fn verify_sha256(file_path: &Path, expected: &str) -> Result<bool> {
    // Validate SHA-256 format: must be exactly 64 hex characters
    if expected.is_empty() {
        anyhow::bail!("SHA-256 hash cannot be empty");
    }

    if expected.len() != 64 {
        anyhow::bail!(
            "Invalid SHA-256 format: expected 64 hex characters, got {}",
            expected.len()
        );
    }

    if !expected.chars().all(|c| c.is_ascii_hexdigit()) {
        anyhow::bail!("SHA-256 hash contains invalid characters: must be hex digits only");
    }

    info!(
        "Verifying SHA-256 of {} against {}",
        file_path.display(),
        expected
    );

    let mut file = File::open(file_path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0; 8192];

    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hasher.update(&buffer[..n]);
    }

    let computed = format!("{:x}", hasher.finalize());
    let matches = computed == expected;

    if matches {
        info!("SHA-256 verification passed");
    } else {
        info!(
            "SHA-256 verification failed: expected {}, got {}",
            expected, computed
        );
    }

    Ok(matches)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::NamedTempFile;

    #[test]
    fn test_sha256_verification() -> Result<()> {
        // Create a temporary file with known content
        let mut file = NamedTempFile::new()?;
        file.write_all(b"hello world")?;
        file.flush()?;

        // Known SHA-256 of "hello world"
        let expected = "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9";

        let result = verify_sha256(file.path(), expected)?;
        assert!(result);

        Ok(())
    }

    #[test]
    fn test_sha256_mismatch() -> Result<()> {
        let mut file = NamedTempFile::new()?;
        file.write_all(b"hello world")?;
        file.flush()?;

        let expected = "0000000000000000000000000000000000000000000000000000000000000000";

        let result = verify_sha256(file.path(), expected)?;
        assert!(!result);

        Ok(())
    }
}
