use anyhow::Result;
use tracing::warn;

use crate::db::Db;
use crate::fetch::FetchError;

pub async fn recorded<T>(db: &Db, result: Result<T, FetchError>) -> Result<T> {
    let error = match result {
        Ok(value) => return Ok(value),
        Err(error) => error,
    };
    if let Err(e) = db.save_fetch_error(&error).await {
        warn!("failed to save fetch error: {e:#}");
    }
    Err(error.into())
}
