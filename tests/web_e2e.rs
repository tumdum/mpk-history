use std::env;
use std::future::Future;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::Path;
use std::time::Duration;

use jiff::tz::TimeZone;
use jiff::{RoundMode, Timestamp, Unit, Zoned, ZonedRound};
use mpk_history::db::{self, Db, LinePositionCount, OpenMode};
use mpk_history::fetch::{self, Source};
use mpk_history::geo::MicroDegrees;
use mpk_history::mpk::{Position, VehicleType};
use mpk_history::web::{self, Clock};
use serde::Deserialize;
use tempfile::TempDir;
use thirtyfour::common::capabilities::chromium::ChromiumLikeCapabilities;
use thirtyfour::manager::BrowserKind;
use thirtyfour::prelude::*;
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

const CHROMEDRIVER_ENV: &str = "CHROMEDRIVER";
const CHROME_ENV: &str = "CHROME";
const CHROME_ARGS: [&str; 3] = ["--headless=new", "--disable-gpu", "--window-size=1200,900"];
const DRIVER_READY_TIMEOUT: Duration = Duration::from_secs(120);
const DATABASE_FILE: &str = "mpk-history.sqlite";
const TITLE: &str = "MPK Wrocław statistics";
const CHROME_PREFERENCES: &str = "prefs";
const ACCEPT_LANGUAGES_PREFERENCE: &str = "intl.accept_languages";
const ENGLISH_BROWSER: &str = "en-US,en";
const POLISH_BROWSER: &str = "pl-PL,pl,en-US,en";

const NOW: i64 = 1_800_000_000;
const FETCH_SECS: i64 = 10;
const HALF_HOUR_SECS: i64 = 30 * 60;
const THREE_HOURS_SECS: i64 = 3 * 60 * 60;
const LATITUDE_E6: i64 = 51_100_000;
const LONGITUDE_E6: i64 = 17_000_000;
const HUNDRED_METERS_NORTH_E6: i64 = 898;
const FIFTY_METERS_NORTH_E6: i64 = 449;

const MINUTE_SECS: i64 = 60;
const HOUR_SECS: i64 = 60 * MINUTE_SECS;
const HOUR_FORMAT: &str = "%H:%M";
const MISSING_VALUE: &str = "–";
const SVG_WIDTH_SCRIPT: &str = "return arguments[0].getBoundingClientRect().width;";
const STEP_POSITION_SCRIPT: &str = "
    const figure = arguments[0];
    const data = JSON.parse(figure.querySelector('.chart-data').textContent);
    const step = data.steps[arguments[1]];
    const box = figure.querySelector('svg').getBoundingClientRect();
    const scale = box.width / data.width;
    return {
        x: Math.round(box.left + step.x * scale),
        y: Math.round(box.top + (data.plot.top + data.plot.bottom) / 2 * scale),
        svg_x: step.x,
        steps: data.steps.length,
    };
";

struct TestServer {
    _dir: TempDir,
    url: String,
    stop: oneshot::Sender<()>,
    task: JoinHandle<anyhow::Result<()>>,
}

impl TestServer {
    async fn start() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join(DATABASE_FILE), OpenMode::CreateIfMissing)
            .await
            .unwrap();
        record_vehicles(&db).await;
        let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
            .await
            .unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let (stop, stopped) = oneshot::channel();
        let clock = Clock::Fixed(Timestamp::from_second(NOW).unwrap());
        let task = tokio::spawn(web::serve(listener, db, clock, async {
            stopped.await.ok();
        }));
        Self {
            _dir: dir,
            url,
            stop,
            task,
        }
    }

    async fn stop(self) {
        self.stop.send(()).ok();
        self.task.await.unwrap().unwrap();
    }
}

async fn in_browser<F, Fut>(test: F)
where
    F: FnOnce(WebDriver, String) -> Fut,
    Fut: Future<Output = WebDriverResult<()>>,
{
    in_browser_speaking(ENGLISH_BROWSER, test).await;
}

async fn in_browser_speaking<F, Fut>(accept_languages: &str, test: F)
where
    F: FnOnce(WebDriver, String) -> Fut,
    Fut: Future<Output = WebDriverResult<()>>,
{
    let server = TestServer::start().await;
    let url = server.url.clone();
    browser(accept_languages)
        .await
        .run_and_quit(|driver| test(driver, url))
        .await
        .unwrap();
    server.stop().await;
}

fn position(vehicle_type: VehicleType, line: &str, course_id: i64, north_e6: i64) -> Position {
    Position {
        vehicle_type,
        line: line.to_owned(),
        course_id,
        latitude: MicroDegrees(LATITUDE_E6 + north_e6),
        longitude: MicroDegrees(LONGITUDE_E6),
    }
}

fn fetch_times(from_unix: i64, to_unix: i64) -> impl Iterator<Item = i64> {
    std::iter::successors(Some(from_unix), |unix| Some(unix + FETCH_SECS))
        .take_while(move |unix| *unix < to_unix)
}

async fn drive(
    db: &Db,
    template: &Position,
    from_unix: i64,
    to_unix: i64,
    north_e6_per_fetch: i64,
) {
    let mut north_e6 = 0;
    for unix in fetch_times(from_unix, to_unix) {
        let position = Position {
            latitude: MicroDegrees(template.latitude.0 + north_e6),
            ..template.clone()
        };
        db.save_positions(unix, &[&position], &[]).await.unwrap();
        north_e6 += north_e6_per_fetch;
    }
}

async fn stand(db: &Db, position: &Position, from_unix: i64, to_unix: i64) {
    let max_row_span_secs = db::duration_secs(db::MAX_ROW_SPAN);
    let mut row: Option<(i64, i64)> = None;
    for unix in fetch_times(from_unix, to_unix) {
        if let Some((row_id, first_seen_unix)) = row
            && unix - first_seen_unix <= max_row_span_secs
        {
            db.save_positions(unix, &[], &[row_id]).await.unwrap();
            continue;
        }
        let ids = db.save_positions(unix, &[position], &[]).await.unwrap();
        row = Some((ids[0], unix));
    }
}

async fn record_vehicles(db: &Db) {
    let recent_bus = position(VehicleType::Bus, "a", 1, 0);
    drive(
        db,
        &recent_bus,
        NOW - HALF_HOUR_SECS,
        NOW,
        HUNDRED_METERS_NORTH_E6,
    )
    .await;
    let tram = position(VehicleType::Tram, "1", 2, 0);
    stand(db, &tram, NOW - HALF_HOUR_SECS, NOW).await;
    let earlier_bus = position(VehicleType::Bus, "b", 3, 0);
    drive(
        db,
        &earlier_bus,
        NOW - THREE_HOURS_SECS,
        NOW - THREE_HOURS_SECS + HALF_HOUR_SECS,
        FIFTY_METERS_NORTH_E6,
    )
    .await;
    record_quality_counts(db).await;
}

async fn record_quality_counts(db: &Db) {
    let closed = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let url = format!("http://{}/", closed.local_addr().unwrap());
    drop(closed);
    let error = fetch::send(Source::MpkPositions, reqwest::Client::new().get(&url))
        .await
        .err()
        .unwrap();
    let recently = NOW - MINUTE_SECS;
    for failed in [None, None, None, Some(&error)] {
        db.save_fetch_outcome(Source::MpkPositions, recently, failed)
            .await
            .unwrap();
    }
    let invalid_buses = LinePositionCount {
        vehicle_type: VehicleType::Bus,
        line: "a".to_owned(),
        received: 20,
        invalid: 1,
    };
    db.save_positions_with_counts(recently, &[], &[], &[invalid_buses])
        .await
        .unwrap();
}

fn non_empty_env(name: &str) -> Option<String> {
    env::var(name).ok().filter(|value| !value.is_empty())
}

async fn browser(accept_languages: &str) -> WebDriver {
    let mut capabilities = DesiredCapabilities::chrome();
    for arg in CHROME_ARGS {
        capabilities.add_arg(arg).unwrap();
    }
    capabilities
        .add_experimental_option(
            CHROME_PREFERENCES,
            serde_json::json!({ ACCEPT_LANGUAGES_PREFERENCE: accept_languages }),
        )
        .unwrap();
    if let Some(chrome) = non_empty_env(CHROME_ENV) {
        capabilities.set_binary(&chrome).unwrap();
    }
    let mut manager = WebDriver::managed(capabilities).ready_timeout(DRIVER_READY_TIMEOUT);
    if let Some(chromedriver) = non_empty_env(CHROMEDRIVER_ENV) {
        manager = manager.driver_binary(BrowserKind::Chrome, Path::new(&chromedriver));
    }
    manager
        .await
        .unwrap_or_else(|e| panic!("cannot start Chrome through chromedriver: {e}"))
}

async fn texts(elements: Vec<WebElement>) -> WebDriverResult<Vec<String>> {
    let mut texts = Vec::with_capacity(elements.len());
    for element in elements {
        texts.push(element.text().await?);
    }
    Ok(texts)
}

async fn attributes(elements: Vec<WebElement>, name: &str) -> WebDriverResult<Vec<String>> {
    let mut values = Vec::with_capacity(elements.len());
    for element in elements {
        values.push(element.attr(name).await?.unwrap_or_default());
    }
    Ok(values)
}

async fn cell(
    driver: &WebDriver,
    kind: &str,
    metric: &str,
    range: &str,
) -> WebDriverResult<String> {
    driver
        .find(By::Css(format!(
            "#stats-{kind} tr[data-metric={metric}] td[data-range={range}]"
        )))
        .await?
        .text()
        .await
}

async fn text(driver: &WebDriver, selector: &str) -> WebDriverResult<String> {
    driver.find(By::Css(selector)).await?.text().await
}

async fn selected_range(driver: &WebDriver) -> WebDriverResult<String> {
    driver
        .find(By::Css("#charts nav a[aria-current=page]"))
        .await?
        .text()
        .await
}

async fn assert_charts_of(driver: &WebDriver, range: &str) -> WebDriverResult<()> {
    let figures = driver.find_all(By::Css("figure.chart")).await?;
    assert_eq!(
        attributes(figures.clone(), "id").await?,
        ["chart-average-speed", "chart-average-vehicles"]
    );
    assert_eq!(
        attributes(figures.clone(), "data-range").await?,
        [range, range]
    );
    for figure in &figures {
        let svg = figure.find(By::Css(".chart-plot svg")).await?;
        let width = driver
            .execute(SVG_WIDTH_SCRIPT, vec![svg.to_json()?])
            .await?;
        assert!(width.json().as_f64().is_some_and(|width| width > 0.0));
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
struct StepPosition {
    x: i64,
    y: i64,
    svg_x: i64,
    steps: usize,
}

async fn step_position(
    driver: &WebDriver,
    figure: &WebElement,
    index: usize,
) -> WebDriverResult<StepPosition> {
    let position = driver
        .execute(STEP_POSITION_SCRIPT, vec![figure.to_json()?, index.into()])
        .await?;
    position.convert()
}

fn hour_label(unix: i64) -> String {
    Timestamp::from_second(unix)
        .unwrap()
        .to_zoned(TimeZone::system())
        .strftime(HOUR_FORMAT)
        .to_string()
}

fn minute_index(start_unix: i64) -> usize {
    usize::try_from((start_unix - (NOW - HOUR_SECS)) / MINUTE_SECS).unwrap()
}

struct Tooltip {
    time: String,
    rows: Vec<(String, String)>,
}

async fn tooltip(figure: &WebElement) -> WebDriverResult<Option<Tooltip>> {
    let tooltip = figure.find(By::Css(".chart-tooltip")).await?;
    if !tooltip.is_displayed().await? {
        return Ok(None);
    }
    let time = tooltip
        .find(By::Css(".chart-tooltip-time"))
        .await?
        .text()
        .await?;
    let mut rows = Vec::new();
    for row in tooltip.find_all(By::Css(".chart-tooltip-row")).await? {
        let name = row
            .find(By::Css(".chart-tooltip-name"))
            .await?
            .text()
            .await?;
        let value = row
            .find(By::Css(".chart-tooltip-value"))
            .await?
            .text()
            .await?;
        rows.push((name, value));
    }
    Ok(Some(Tooltip { time, rows }))
}

async fn tooltip_time(figure: &WebElement) -> WebDriverResult<Option<String>> {
    Ok(tooltip(figure).await?.map(|tooltip| tooltip.time))
}

fn rows(expected: &[(&str, &str)]) -> Vec<(String, String)> {
    expected
        .iter()
        .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
        .collect()
}

async fn hover_step(
    driver: &WebDriver,
    chart: &str,
    start_unix: i64,
) -> WebDriverResult<(WebElement, Tooltip)> {
    let (figure, shown, steps) = hover_step_at(driver, chart, minute_index(start_unix)).await?;
    assert_eq!(steps, minute_index(NOW), "one step per minute");
    Ok((figure, shown))
}

async fn hover_step_at(
    driver: &WebDriver,
    chart: &str,
    index: usize,
) -> WebDriverResult<(WebElement, Tooltip, usize)> {
    let figure = driver.find(By::Id(format!("chart-{chart}"))).await?;
    figure.scroll_into_view().await?;
    let position = step_position(driver, &figure, index).await?;
    driver
        .action_chain()
        .move_to(position.x, position.y)
        .perform()
        .await?;
    let shown = tooltip(&figure)
        .await?
        .unwrap_or_else(|| panic!("{chart}: no tooltip at {position:?}"));
    let overlay = figure.find(By::Css("g.chart-hover")).await?;
    assert_eq!(overlay.attr("display").await?, None, "{chart}");
    let crosshair = overlay.find(By::Css("line.chart-crosshair")).await?;
    assert_eq!(
        crosshair.attr("x1").await?,
        Some(position.svg_x.to_string()),
        "{chart}"
    );
    Ok((figure, shown, position.steps))
}

async fn step_count(driver: &WebDriver, chart: &str) -> WebDriverResult<usize> {
    let figure = driver.find(By::Id(format!("chart-{chart}"))).await?;
    Ok(step_position(driver, &figure, 0).await?.steps)
}

async fn current_path(driver: &WebDriver) -> WebDriverResult<String> {
    Ok(driver.current_url().await?.path().to_owned())
}

async fn period_cell(driver: &WebDriver, kind: &str, metric: &str) -> WebDriverResult<String> {
    driver
        .find(By::Css(format!(
            "#period-stats tr[data-metric={metric}] td[data-kind={kind}]"
        )))
        .await?
        .text()
        .await
}

fn local(unix: i64) -> Zoned {
    Timestamp::from_second(unix)
        .unwrap()
        .to_zoned(TimeZone::system())
}

async fn visible_dots(figure: &WebElement) -> WebDriverResult<usize> {
    let mut visible = 0;
    for dot in figure.find_all(By::Css("circle.chart-dot")).await? {
        if dot.is_displayed().await? {
            visible += 1;
        }
    }
    Ok(visible)
}

#[tokio::test]
async fn summary_shows_statistics_per_kind_and_range() {
    in_browser(|driver, url| async move {
        driver.goto(url).await?;
        assert_eq!(driver.title().await?, TITLE);
        let tables = driver.find_all(By::Css("#summary table.stats")).await?;
        assert_eq!(
            attributes(tables, "id").await?,
            ["stats-all", "stats-bus", "stats-tram"]
        );
        let captions = driver
            .find_all(By::Css("#summary table.stats caption"))
            .await?;
        assert_eq!(texts(captions).await?, ["All vehicles", "Buses", "Trams"]);

        let expected = [
            ("bus", "average-speed", "hour", "36.0"),
            ("bus", "average-speed", "day", "27.0"),
            ("tram", "average-speed", "hour", "0.0"),
            ("all", "average-vehicles", "hour", "2.0"),
            ("all", "average-vehicles", "day", "1.5"),
            ("bus", "average-vehicles", "day", "1.0"),
            ("all", "peak-vehicles", "day", "2"),
            ("bus", "peak-vehicles", "week", "1"),
            ("bus", "distance", "month", "27"),
            ("tram", "distance", "month", "0"),
        ];
        for (kind, metric, range, value) in expected {
            assert_eq!(
                cell(&driver, kind, metric, range).await?,
                value,
                "{kind} {metric} {range}"
            );
        }
        assert!(driver.find_all(By::Id("fetch-failures")).await?.is_empty());
        Ok(())
    })
    .await;
}

#[tokio::test]
async fn range_links_switch_the_charts() {
    in_browser(|driver, url| async move {
        driver.goto(url).await?;
        assert_eq!(selected_range(&driver).await?, "Last day");
        assert_charts_of(&driver, "day").await?;

        driver
            .find(By::LinkText("Last week"))
            .await?
            .click()
            .await?;
        assert_eq!(driver.current_url().await?.query(), Some("range=week"));
        assert_eq!(selected_range(&driver).await?, "Last week");
        assert_charts_of(&driver, "week").await?;
        let captions = texts(driver.find_all(By::Css("figcaption")).await?).await?;
        assert!(
            captions
                .iter()
                .all(|caption| caption.ends_with("last week")),
            "{captions:?}"
        );
        Ok(())
    })
    .await;
}

#[tokio::test]
async fn charts_draw_a_line_per_kind() {
    in_browser(|driver, url| async move {
        for chart in ["average-vehicles", "average-speed"] {
            driver
                .goto(format!("{url}/charts/{chart}.svg?range=hour"))
                .await?;
            let svg = driver.find(By::Tag("svg")).await?;
            let labels = texts(svg.find_all(By::Tag("text")).await?).await?;
            for series in ["All vehicles", "Buses", "Trams"] {
                assert!(
                    labels.iter().any(|label| label == series),
                    "{chart}: {labels:?}"
                );
            }
            assert!(!labels.iter().any(|label| label == "Trains"), "{chart}");
            assert!(
                !svg.find_all(By::Tag("polyline")).await?.is_empty(),
                "{chart}"
            );
        }
        Ok(())
    })
    .await;
}

#[tokio::test]
async fn hovering_a_chart_shows_the_values_of_that_step() {
    in_browser(|driver, url| async move {
        driver.goto(format!("{url}/?range=hour")).await?;

        let vehicles_at = NOW - 10 * MINUTE_SECS;
        let (vehicles, shown) = hover_step(&driver, "average-vehicles", vehicles_at).await?;
        assert_eq!(shown.time, hour_label(vehicles_at));
        assert_eq!(
            shown.rows,
            rows(&[("All vehicles", "2.0"), ("Buses", "1.0"), ("Trams", "1.0")])
        );
        assert_eq!(visible_dots(&vehicles).await?, 3);

        let before_data_at = NOW - 45 * MINUTE_SECS;
        let (_, shown) = hover_step(&driver, "average-vehicles", before_data_at).await?;
        assert_eq!(shown.time, hour_label(before_data_at));
        assert_eq!(
            shown.rows,
            rows(&[
                ("All vehicles", MISSING_VALUE),
                ("Buses", MISSING_VALUE),
                ("Trams", MISSING_VALUE)
            ])
        );
        assert_eq!(visible_dots(&vehicles).await?, 0);

        let speed_at = NOW - 15 * MINUTE_SECS;
        let (speed, shown) = hover_step(&driver, "average-speed", speed_at).await?;
        assert_eq!(shown.time, hour_label(speed_at));
        assert_eq!(
            shown.rows,
            rows(&[
                ("All vehicles", "18.0"),
                ("Buses", "36.0"),
                ("Trams", "0.0")
            ])
        );
        assert_eq!(visible_dots(&speed).await?, 3);
        assert!(
            tooltip(&vehicles).await?.is_none(),
            "only one tooltip at a time"
        );

        let heading = driver.find(By::Tag("h2")).await?;
        driver
            .action_chain()
            .move_to_element_center(&heading)
            .perform()
            .await?;
        assert!(
            tooltip(&speed).await?.is_none(),
            "tooltip stays after leaving"
        );
        Ok(())
    })
    .await;
}

#[tokio::test]
async fn arrow_keys_move_between_chart_steps() {
    in_browser(|driver, url| async move {
        driver.goto(format!("{url}/?range=hour")).await?;
        let figure = driver.find(By::Id("chart-average-vehicles")).await?;
        figure.scroll_into_view().await?;
        figure
            .find(By::Css(".chart-plot svg"))
            .await?
            .focus()
            .await?;
        assert_eq!(tooltip_time(&figure).await?, None);

        let first = NOW - HOUR_SECS;
        driver
            .action_chain()
            .send_keys(Key::Right)
            .perform()
            .await?;
        assert_eq!(tooltip_time(&figure).await?, Some(hour_label(first)));
        driver
            .action_chain()
            .send_keys(Key::Right)
            .perform()
            .await?;
        assert_eq!(
            tooltip_time(&figure).await?,
            Some(hour_label(first + MINUTE_SECS))
        );
        driver.action_chain().send_keys(Key::Left).perform().await?;
        driver.action_chain().send_keys(Key::Left).perform().await?;
        assert_eq!(tooltip_time(&figure).await?, Some(hour_label(first)));
        driver
            .action_chain()
            .send_keys(Key::Escape)
            .perform()
            .await?;
        assert_eq!(tooltip_time(&figure).await?, None);
        driver.action_chain().send_keys(Key::Left).perform().await?;
        assert_eq!(
            tooltip_time(&figure).await?,
            Some(hour_label(NOW - MINUTE_SECS))
        );
        Ok(())
    })
    .await;
}

#[tokio::test]
async fn calendar_pages_show_statistics_of_their_period() {
    in_browser(|driver, url| async move {
        driver.goto(url).await?;
        driver
            .find(By::LinkText("This week"))
            .await?
            .click()
            .await?;
        let week_path = current_path(&driver).await?;
        let week = local(NOW).iso_week_date();
        assert_eq!(
            week_path,
            format!("/week/{:04}-W{:02}", week.year(), week.week())
        );
        assert_eq!(
            driver.find(By::Tag("h1")).await?.text().await?,
            format!("Week {}, {}", week.week(), week.year())
        );
        let expected = [
            ("bus", "average-speed", "27.0"),
            ("all", "average-vehicles", "1.5"),
            ("bus", "average-vehicles", "1.0"),
            ("all", "peak-vehicles", "2"),
            ("bus", "distance", "27"),
            ("tram", "distance", "0"),
        ];
        for (kind, metric, value) in expected {
            assert_eq!(
                period_cell(&driver, kind, metric).await?,
                value,
                "{kind} {metric}"
            );
        }
        assert_eq!(driver.find_all(By::Css("#parts a")).await?.len(), 7);
        driver.find(By::Id("quality-link")).await?.click().await?;
        assert_eq!(current_path(&driver).await?, format!("{week_path}/quality"));
        assert_eq!(
            text(&driver, "#fetch-failures tr[data-source=mpk-positions] td").await?,
            "25.0%"
        );
        assert_eq!(
            text(&driver, "#invalid-positions tr[data-kind=all] td").await?,
            "5.0%"
        );
        driver
            .find(By::Id("statistics-link"))
            .await?
            .click()
            .await?;
        assert_eq!(current_path(&driver).await?, week_path);

        let last = step_count(&driver, "average-vehicles").await? - 1;
        let (_, shown, _) = hover_step_at(&driver, "average-vehicles", last).await?;
        let last_hour = local(NOW - 1)
            .round(
                ZonedRound::new()
                    .smallest(Unit::Hour)
                    .mode(RoundMode::Trunc),
            )
            .unwrap();
        assert_eq!(shown.time, last_hour.strftime("%a %d.%m %H:%M").to_string());
        assert_eq!(
            shown.rows,
            rows(&[("All vehicles", "2.0"), ("Buses", "1.0"), ("Trams", "1.0")])
        );

        let drive_day = local(NOW - FETCH_SECS).date();
        let day_path = format!("/day/{}", drive_day.strftime("%Y-%m-%d"));
        driver
            .find(By::Css(format!("#parts a[href='{day_path}']")))
            .await?
            .click()
            .await?;
        assert_eq!(current_path(&driver).await?, day_path);
        assert_eq!(
            driver.find(By::Tag("h1")).await?.text().await?,
            drive_day.strftime("%A, %-d %B %Y").to_string()
        );
        driver
            .find(By::Css(format!(".breadcrumbs a[href='{week_path}']")))
            .await?;

        driver.find(By::Id("previous")).await?.click().await?;
        let previous_day = drive_day.yesterday().unwrap();
        assert_eq!(
            current_path(&driver).await?,
            format!("/day/{}", previous_day.strftime("%Y-%m-%d"))
        );
        assert_eq!(
            driver.find(By::Id("next")).await?.attr("href").await?,
            Some(day_path)
        );
        Ok(())
    })
    .await;
}

async fn click(driver: &WebDriver, selector: &str) -> WebDriverResult<()> {
    driver.find(By::Css(selector)).await?.click().await
}

async fn heading(driver: &WebDriver) -> WebDriverResult<String> {
    driver.find(By::Tag("h1")).await?.text().await
}

#[tokio::test]
async fn vehicle_and_line_links_switch_what_is_counted() {
    in_browser(|driver, url| async move {
        driver.goto(format!("{url}/?range=week")).await?;
        click(&driver, "#vehicles a[href='/bus?range=week']").await?;
        assert_eq!(current_path(&driver).await?, "/bus");
        assert_eq!(heading(&driver).await?, "Buses");
        assert_eq!(selected_range(&driver).await?, "Last week");
        assert_eq!(
            texts(driver.find_all(By::Css("#lines a")).await?).await?,
            ["a", "b"]
        );

        click(&driver, "#lines a[href='/bus/a?range=week']").await?;
        assert_eq!(current_path(&driver).await?, "/bus/a");
        assert_eq!(heading(&driver).await?, "Bus a");
        assert_eq!(selected_range(&driver).await?, "Last week");
        assert_eq!(
            text(
                &driver,
                "#stats-bus-a tr[data-metric=average-speed] td[data-range=week]"
            )
            .await?,
            "36.0"
        );

        driver
            .find(By::LinkText("This week"))
            .await?
            .click()
            .await?;
        let week = local(NOW).iso_week_date();
        let week_path = format!("/week/{:04}-W{:02}", week.year(), week.week());
        assert_eq!(current_path(&driver).await?, format!("/bus/a{week_path}"));
        assert_eq!(
            heading(&driver).await?,
            format!("Bus a: Week {}, {}", week.week(), week.year())
        );
        assert_eq!(
            period_cell(&driver, "bus-a", "average-speed").await?,
            "36.0"
        );
        assert_eq!(
            period_cell(&driver, "bus-a", "average-vehicles").await?,
            "1.0"
        );
        let (_, shown, _) = hover_step_at(
            &driver,
            "average-vehicles",
            step_count(&driver, "average-vehicles").await? - 1,
        )
        .await?;
        assert_eq!(shown.rows, rows(&[("Bus a", "1.0")]));

        driver.find(By::Id("quality-link")).await?.click().await?;
        assert_eq!(
            current_path(&driver).await?,
            format!("/bus/a{week_path}/quality")
        );
        assert_eq!(
            text(&driver, "#invalid-positions tr[data-kind=bus-a] td").await?,
            "5.0%"
        );
        driver
            .find(By::Id("statistics-link"))
            .await?
            .click()
            .await?;
        assert_eq!(current_path(&driver).await?, format!("/bus/a{week_path}"));

        click(&driver, &format!("#vehicles a[href='/tram{week_path}']")).await?;
        assert_eq!(current_path(&driver).await?, format!("/tram{week_path}"));
        assert_eq!(period_cell(&driver, "tram", "average-speed").await?, "0.0");
        click(&driver, &format!("#vehicles a[href='{week_path}']")).await?;
        assert_eq!(current_path(&driver).await?, week_path);
        assert_eq!(period_cell(&driver, "bus", "average-speed").await?, "27.0");
        Ok(())
    })
    .await;
}

#[tokio::test]
async fn quality_page_shows_when_downloads_failed() {
    in_browser(|driver, url| async move {
        driver.goto(url).await?;
        driver
            .find(By::LinkText("Data quality"))
            .await?
            .click()
            .await?;
        assert_eq!(current_path(&driver).await?, "/quality");
        let expected = [
            (
                "#fetch-failures tr[data-source=mpk-positions] td[data-range=hour]",
                "25.0%",
            ),
            (
                "#fetch-failures tr[data-source=open-meteo] td[data-range=hour]",
                MISSING_VALUE,
            ),
            (
                "#invalid-positions tr[data-kind=bus] td[data-range=day]",
                "5.0%",
            ),
        ];
        for (selector, value) in expected {
            assert_eq!(text(&driver, selector).await?, value, "{selector}");
        }
        assert!(
            !text(&driver, "main").await?.contains(" of "),
            "counts must not be shown"
        );

        driver
            .find(By::Css("#charts nav a[href='/quality?range=hour']"))
            .await?
            .click()
            .await?;
        let failed_at = NOW - MINUTE_SECS;
        let (_, shown) = hover_step(&driver, "failed-downloads", failed_at).await?;
        assert_eq!(shown.time, hour_label(failed_at));
        assert_eq!(shown.rows, rows(&[("MPK positions", "25.0%")]));
        let (_, shown) = hover_step(&driver, "invalid-positions", failed_at).await?;
        assert_eq!(
            shown.rows,
            rows(&[("All vehicles", "5.0%"), ("Buses", "5.0%")])
        );
        let (_, shown) = hover_step(&driver, "failed-downloads", failed_at - MINUTE_SECS).await?;
        assert_eq!(shown.rows, rows(&[("MPK positions", MISSING_VALUE)]));
        Ok(())
    })
    .await;
}

#[tokio::test]
async fn pages_follow_the_language_of_the_browser() {
    in_browser_speaking(POLISH_BROWSER, |driver, url| async move {
        driver.goto(format!("{url}/?range=hour")).await?;
        assert_eq!(driver.title().await?, "Statystyki MPK Wrocław");
        let page = driver.find(By::Tag("html")).await?;
        assert_eq!(page.attr("lang").await?.as_deref(), Some("pl"));
        assert_eq!(selected_range(&driver).await?, "Ostatnia godzina");
        assert_eq!(cell(&driver, "bus", "average-speed", "day").await?, "27,0");

        let speed_at = NOW - 15 * MINUTE_SECS;
        let (_, shown) = hover_step(&driver, "average-speed", speed_at).await?;
        assert_eq!(shown.time, hour_label(speed_at));
        assert_eq!(
            shown.rows,
            rows(&[
                ("Wszystkie pojazdy", "18,0"),
                ("Autobusy", "36,0"),
                ("Tramwaje", "0,0")
            ])
        );

        click(&driver, "#history a").await?;
        assert!(current_path(&driver).await?.starts_with("/day/"));
        assert_eq!(text(&driver, "#previous").await?, "← Poprzedni dzień");
        driver
            .find(By::LinkText("Jakość danych"))
            .await?
            .click()
            .await?;
        assert!(heading(&driver).await?.starts_with("Jakość danych: "));
        Ok(())
    })
    .await;
}
