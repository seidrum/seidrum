use super::PackageArtifact;
use anyhow::{anyhow, Result};
use futures_util::stream::StreamExt;
use indicatif::{ProgressBar, ProgressStyle};
use std::fs::File;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use tracing::{debug, info};

/// Validate artifact URL before downloading
fn validate_artifact_url(url: &str) -> Result<()> {
    // Allow https:// (and http:// for localhost dev)
    if !url.starts_with("https://") && !url.starts_with("http://localhost") && !url.starts_with("http://127.0.0.1") {
        anyhow::bail!("Artifact URL must use https:// or be localhost for development: {}", url);
    }

    // Check for private/internal IP ranges (basic string-based check)
    let private_ranges = [
        "10.",           // 10.0.0.0/8
        "172.16.",       // 172.16.0.0/12
        "172.17.",
        "172.18.",
        "172.19.",
        "172.20.",
        "172.21.",
        "172.22.",
        "172.23.",
        "172.24.",
        "172.25.",
        "172.26.",
        "172.27.",
        "172.28.",
        "172.29.",
        "172.30.",
        "172.31.",
        "192.168.",      // 192.168.0.0/16
        "169.254.",      // 169.254.0.0/16 (link-local)
    ];

    for range in &private_ranges {
        if url.contains(range) && !url.contains("localhost") && !url.contains("127.0.0.1") {
            anyhow::bail!("Artifact URL uses private/internal IP range, which is not allowed: {}", url);
        }
    }

    Ok(())
}

/// Detect current target platform triple
pub fn current_target() -> &'static str {
    #[cfg(all(target_arch = "x86_64", target_os = "macos"))]
    return "x86_64-apple-darwin";
    #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
    return "aarch64-apple-darwin";
    #[cfg(all(target_arch = "x86_64", target_os = "linux"))]
    return "x86_64-unknown-linux-gnu";
    #[cfg(all(target_arch = "aarch64", target_os = "linux"))]
    return "aarch64-unknown-linux-gnu";
    #[cfg(all(target_arch = "x86_64", target_os = "windows"))]
    return "x86_64-pc-windows-gnu";
    #[cfg(all(target_arch = "aarch64", target_os = "windows"))]
    return "aarch64-pc-windows-gnu";

    // Fallback for unsupported platforms
    "unknown-unknown-unknown"
}

/// Download artifact for current platform to destination
pub async fn download_artifact(
    artifacts: &[PackageArtifact],
    dest_dir: &Path,
) -> Result<PathBuf> {
    let target = current_target();
    debug!("Looking for artifact for target: {}", target);

    let artifact = artifacts
        .iter()
        .find(|a| a.target == target)
        .ok_or_else(|| {
            anyhow!(
                "No artifact found for target {}. Available targets: {}",
                target,
                artifacts
                    .iter()
                    .map(|a| format!("{} ({})", a.target, a.url))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })?;

    // Validate URL
    validate_artifact_url(&artifact.url)?;

    info!("Downloading artifact from {}", artifact.url);

    let client = reqwest::Client::new();
    let response = client
        .get(&artifact.url)
        .send()
        .await?;

    let total_size = response.content_length().unwrap_or(0);
    let pb = if total_size > 0 {
        let bar = ProgressBar::new(total_size);
        bar.set_style(
            ProgressStyle::default_bar()
                .template("{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {bytes}/{total_bytes} ({eta})")?
                .progress_chars("#>-"),
        );
        Some(bar)
    } else {
        None
    };

    let mut stream = response.bytes_stream();
    let filename = artifact
        .url
        .split('/')
        .last()
        .unwrap_or("package.tar.gz");
    let dest_path = dest_dir.join(filename);

    let mut file = File::create(&dest_path)?;

    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        file.write_all(&chunk)?;
        if let Some(ref bar) = pb {
            bar.inc(chunk.len() as u64);
        }
    }

    if let Some(bar) = pb {
        bar.finish_with_message("Download complete");
    }

    info!("Artifact downloaded to {}", dest_path.display());
    Ok(dest_path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_current_target_not_empty() {
        let target = current_target();
        assert!(!target.is_empty());
        assert!(!target.contains("unknown-unknown"));
    }
}
