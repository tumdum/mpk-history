use anyhow::Result;
use tracing::info;

use crate::db::Db;
use crate::fetch::Source;
use crate::fetch_errors;
use crate::open_meteo::{Area, Client};

pub struct WeatherCollector {
    client: Client,
    db: Db,
    area: Area,
    model: String,
}

impl WeatherCollector {
    #[must_use]
    pub fn new(client: Client, db: Db, area: Area, model: String) -> Self {
        Self {
            client,
            db,
            area,
            model,
        }
    }

    pub async fn collect(&mut self) -> Result<()> {
        let fetched_unix = jiff::Timestamp::now().as_second();
        let points = fetch_errors::recorded(
            &self.db,
            Source::OpenMeteo,
            self.client.fetch_current(&self.area, &self.model),
        )
        .await?;
        let inserted = self
            .db
            .save_weather(fetched_unix, &self.model, &points)
            .await?;
        info!(points = points.len(), new = inserted, "saved weather");
        Ok(())
    }
}
