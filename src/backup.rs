use std::path::Path;
use std::time::Instant;

use anyhow::{Result, bail};

use crate::db::Db;

const BYTES_PER_MEGABYTE: f64 = 1024.0 * 1024.0;

pub async fn backup(db: &Db, target: &Path) -> Result<()> {
    if target.exists() {
        bail!(
            "{} already exists, refusing to overwrite it",
            target.display()
        );
    }
    let started = Instant::now();
    db.backup_to(target).await?;
    #[allow(clippy::cast_precision_loss)]
    let megabytes = std::fs::metadata(target)?.len() as f64 / BYTES_PER_MEGABYTE;
    println!(
        "backed up to {} ({:.1} MB) in {:.1} s",
        target.display(),
        megabytes,
        started.elapsed().as_secs_f64()
    );
    Ok(())
}
