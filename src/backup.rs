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
    let size = std::fs::metadata(target)?.len();
    println!(
        "backed up to {} ({:.1} MB) in {:.1} s",
        target.display(),
        size as f64 / BYTES_PER_MEGABYTE,
        started.elapsed().as_secs_f64()
    );
    Ok(())
}
