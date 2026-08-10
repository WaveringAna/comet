//! `nova update` — check for and apply a newer release, natively (the same
//! managed install flow: download → verify → symlink swap →
//! service restart). macOS app bundles swap the bundle instead; source builds
//! are report-only.

use anyhow::bail;
use nova_update::{InstallKind, current_version, version_newer};

/// `--check` prints the verdict and exits (nonzero when an update is available,
/// so scripts can gate on it).
pub async fn update(release_url: &str, check_only: bool) -> anyhow::Result<()> {
    let manifest = nova_update::fetch_latest(release_url).await?;
    let current = current_version();
    if !version_newer(&manifest.version, current) {
        println!(
            "nova {current} is up to date (latest: {}).",
            manifest.version
        );
        return Ok(());
    }
    println!("nova {current} → {} available", manifest.version);
    if check_only {
        std::process::exit(1);
    }

    match nova_update::detect_install() {
        InstallKind::Managed { app_root } => {
            println!(
                "downloading {}…",
                nova_update::headless_artifact(&manifest.version)
            );
            nova_update::stage_headless(release_url, &manifest, &app_root).await?;
            nova_update::apply_headless(&app_root, &manifest.version)?;
            println!(
                "installed {} (current → {})",
                app_root.join(&manifest.version).display(),
                manifest.version
            );
            match nova_update::restart_service(&app_root) {
                Ok(()) => println!("engine service restarted."),
                Err(err) => println!(
                    "note: service restart failed ({err:#}) — restart the engine manually to finish."
                ),
            }
            Ok(())
        }
        InstallKind::MacApp { bundle } => {
            println!(
                "downloading {}…",
                nova_update::mac_app_artifact(&manifest.version)
            );
            let data_dir = super::data_dir_from_env();
            let staged = nova_update::stage_mac_app(release_url, &manifest, &data_dir).await?;
            nova_update::apply_mac_app(&staged, &bundle)?;
            println!("updated {} — relaunch Nova to finish.", bundle.display());
            Ok(())
        }
        InstallKind::Unmanaged => {
            bail!(
                "this binary is not update-managed (source build or hand-copied).\n\
                 Download the latest Nova release for this platform, or rebuild from source."
            )
        }
    }
}
