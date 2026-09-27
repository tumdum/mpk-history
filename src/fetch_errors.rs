use std::future::Future;

use anyhow::Result;
use tracing::warn;

use crate::db::Db;
use crate::fetch::{FetchError, Source};

pub async fn recorded<T>(
    db: &Db,
    source: Source,
    fetch: impl Future<Output = Result<T, FetchError>>,
) -> Result<T> {
    let started_unix = jiff::Timestamp::now().as_second();
    let result = fetch.await;
    let error = result.as_ref().err();
    let started_unix = error.map_or(started_unix, |error| error.attempt.started_unix);
    if let Err(e) = db.save_fetch_outcome(source, started_unix, error).await {
        warn!("failed to save fetch outcome: {e:#}");
    }
    result.map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::db::OpenMode;

    const IN_MEMORY: &str = ":memory:";
    const VALUE: i64 = 7;

    #[tokio::test]
    async fn records_successes_and_failures() {
        let db = Db::open(Path::new(IN_MEMORY), OpenMode::CreateIfMissing)
            .await
            .unwrap();
        let before = jiff::Timestamp::now().as_second();
        let value = recorded(&db, Source::OpenData, async { Ok(VALUE) })
            .await
            .unwrap();
        assert_eq!(value, VALUE);

        let url = crate::fetch::tests::refused_url();
        let request = reqwest::Client::new().get(&url);
        let failed = recorded(
            &db,
            Source::OpenData,
            crate::fetch::send(Source::OpenData, request),
        )
        .await;
        assert!(failed.is_err());

        let counts = db.fetch_counts(before - 60, before + 3600).await.unwrap();
        assert_eq!(counts.len(), 1);
        assert_eq!((counts[0].attempts, counts[0].failures), (2, 1));
    }
}
