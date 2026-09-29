# Web page

`mpk-history collect` also starts a small web server with statistics and charts. It
is on by default:

```sh
mpk-history collect --listen 127.0.0.1:8080
```

| Option | Env | Default | Meaning |
|---|---|---|---|
| `--web` | `MPK_WEB` | `enabled` | `disabled` runs the collector only |
| `--listen` | `MPK_LISTEN` | `127.0.0.1:8080` | Address of the web page |
| `--map` | `MPK_MAP` | `enabled` | `disabled` doesn't download the map, so pages have no map |

The address is bound before collecting starts, so a port in use stops `collect` at
once. The web page shares the database connection with the collector. It reads rows in
6-hour chunks, so a save waits at most for one chunk.

## Pages

| Path | What it returns |
|---|---|
| `/` | Tables with statistics, and the charts for one time range |
| `/?range=hour` | The same page with charts for another range: `hour`, `day` (default), `week`, `month` |
| `/day/2026-09-27` | One calendar day |
| `/week/2026-W39` | One ISO week, Monday to Sunday |
| `/month/2026-09` | One calendar month |
| `/year/2026` | One calendar year |
| `/quality?range=…` | Data quality: failed downloads and invalid GPS positions |
| `/day/2026-09-27/quality`, `/week/…/quality`, … | Data quality of one calendar period |
| `/charts/average-speed.svg?range=…` | The average speed chart as a plain SVG image |
| `/charts/average-vehicles.svg?range=…` | The average active vehicles chart as a plain SVG image |
| `/map.svg` | The map of current vehicles as a plain SVG image |
| `/map/wroclaw.jpg` | The map of Wrocław without vehicles, used by `/map.svg` |

An unknown `range` gives `400`, an unknown chart `404`. A period in any other form,
like `/day/2026-9-27` or `/week/2026-W54`, gives `404`. Charts are drawn on the server
with `plotters`. Every page starts with its charts, average speed first. The overview
pages (`/`, `/bus`, `/bus/136`, …) then show the [map of current
vehicles](#map-of-current-vehicles), and all pages continue with the navigation links
and the tables.

Each page has a `<meta name="description">` for search engines: its heading followed by
one translated sentence, one for statistics pages and one for data quality pages.
Every page also loads the Google Analytics tag (`gtag.js`, id `G-ZCQG7QP403`) in its
`<head>`.

Chart colors are chosen so that every pair of lines in a chart stays apart for the
common kinds of color blindness, since any two lines can cross. The vehicle charts use
blue (all vehicles), orange (buses), aqua (trams) and violet (trains); the download
chart uses magenta, green, violet and yellow. Both sets were checked with a palette
validator (OKLab distance of at least 8 under simulated protanopia, deuteranopia and
tritanopia, and at least 15 with normal vision). plotters' own palettes don't pass: their
yellow and pink are too light on the white chart. Swapping a color means checking the
whole set again. Every chart with two or more lines also has a legend, and hovering
names each value, so color is never the only cue.

## Languages

The pages are in English and Polish. Each request picks the language from the
browser's `Accept-Language` header: the supported language with the highest weight
wins (`pl-PL` counts as `pl`), and English is used when none of the listed languages is
supported or the header is missing. The same URL serves both, so responses carry
`Content-Language` and `Vary: Accept-Language`, and `<html lang>` is set. Chart images
(`/charts/…`) follow the header too.

All text lives in `src/web/i18n/`, one file per language (`en.rs`, `pl.rs`). Each one
implements the `Translation` trait from `src/web/i18n.rs`, so a missing string is a
compile error, not an English leftover. Besides plain strings it builds the text that
differs between languages in form, not just in words:

- Dates: jiff only knows English names, so Polish has its own weekday and month names,
  in the nominative ("wrzesień 2026") and in the genitive inside a date ("27 września
  2026").
- Numbers use the language's decimal separator (`36,0`, `33,3%`), in tables, hover
  values and chart axes.
- Whole phrases per period kind ("Statystyki tego tygodnia", "← Poprzedni miesiąc")
  instead of a noun glued into a sentence, so each language can inflect it.

Period titles are written as they read inside a sentence; in Polish that starts in
lower case ("Autobus 136: wrzesień 2026"). The code capitalizes them where they stand
alone: headings, breadcrumbs and the list of days or months.

To add a language, add a variant to `Language` with its tag, and a file implementing
`Translation`.

## Vehicles and lines

Every statistics page and chart above can be limited to one vehicle type or one line
by putting it in front of the path:

| Prefix | Counts | Examples |
|---|---|---|
| none | all vehicles | `/`, `/week/2026-W39` |
| `/bus`, `/tram`, `/train` | all vehicles of that type | `/tram?range=week`, `/bus/day/2026-09-26` |
| `/bus/136`, `/tram/74`, … | one line | `/bus/136?range=week`, `/bus/136/month/2027-05` |

The line is the name from the MPK feed, under its vehicle type, so a bus and a tram
with the same name are two lines. A line that is not in the `lines` table gives `404`.
A known line with nothing in the period shows `–`. Data quality pages take the same
prefixes (see [Data quality](#data-quality)).

Each page has a "Vehicles" row with links to all vehicles and each vehicle type, and a
"Lines" section with the lines seen in the page's time range (the last month on the
overview). The links keep the current period or chart range, so from
`/bus/136/week/2026-W39` "Trams" goes to `/tram/week/2026-W39`. Pages of a type or a
line have one table and one line per chart.

For a line, every clock hour (local time) in which none of its vehicles was seen is
left out: its fetch times don't count, so the average active vehicles is the mean over
the hours the line ran, and a line that runs only at night isn't averaged down by the
day. Charts of a line have gaps in those hours. Vehicle types and all vehicles count
every fetch time.

## Calendar pages

Days, weeks, months and years start at local midnight in the server's time zone, so a
day can be 23 or 25 hours long when the clocks change. Each page shows one table with
the totals of the period (a column per vehicle type with vehicles in it), the two
charts over the whole period, links to the previous and next period, and links to the
week, month and year it belongs to. A week belongs to the month and year of its
Thursday, like ISO weeks do. Week and month pages link to their days, year pages to
their months. The overview links to today, yesterday, this and the previous week and
month, and this year. A period that hasn't ended yet says "So far".

| Page | Chart step |
|---|---|
| Day | 15 minutes |
| Week | 1 hour |
| Month | 6 hours |
| Year | 1 day |

## Data quality

Data quality has its own pages: `/quality` for the last hour, day, week and month, and
`<period>/quality` for each calendar page. Both take the vehicle type and line prefixes
of the statistics pages, like `/bus/136/quality` or `/tram/week/2026-W39/quality`. The
statistics pages link to them with "Data quality", and back with "Statistics of this …".
Each has two tables and a chart for each:

- **Failed downloads:** for each source (MPK positions, MPK line list, Open-Meteo,
  open data fleet), the share of downloads that failed. Any failure that gets a
  `fetch_errors` row counts: timeouts, connection errors, HTTP errors and answers that
  can't be read.
- **Invalid GPS positions:** for all vehicles and each vehicle type (or only the
  selected type or line), the share of positions in the MPK feed with coordinates the
  collector threw away.

Pages of a vehicle type or a line show only invalid GPS positions. Failed downloads
are left out there: one MPK download has the positions of all lines, so a failure is
never about one line or type.

Only percentages are shown, never the counts. Shares under 0.1% show as `<0.1%`, so a
few failures don't look like none. The charts have one line per source or vehicle type
with the share of each chart step (the same steps as the statistics charts). A step
without any download or position is a gap, not 0%: sources fetched rarely, like
Open-Meteo every 30 minutes, show as separate dots on the shorter steps.

The numbers come from `fetch_attempts` and `position_counts` (see
[database.md](database.md#fetch_attempts)), which the collector fills since migration
`0014`, and for lines from `line_position_counts`, filled since migration `0015`.
Periods before that, or when a collector was disabled, show `–`.

## Map of current vehicles

Below the charts, the overview pages show a map of Wrocław with every vehicle seen in
the last 30 seconds: buses as orange circles, trams as green squares and trains as
violet triangles, in the colors of the charts. A vehicle type or line page shows only
its own vehicles. Under the map is a legend with the number of vehicles of each type
on it, or "No vehicles right now", and the OpenStreetMap attribution.

- **Base map:** at start the server downloads the OpenStreetMap tiles of the city
  (zoom 12, 25 tiles from `tile.openstreetmap.org`, see
  [data-sources.md](data-sources.md#openstreetmap-tiles)), joins them into one image
  (about 1080×950 px), fades it to light gray so that vehicles stand out, and keeps it
  in memory as a JPEG (about 200 KB). It is served at `/map/wroclaw.jpg`. When the
  download fails it is tried again every 15 minutes. Until it succeeds, pages have no
  map and `/map.svg` and `/map/wroclaw.jpg` give `503`.
- **Vehicles:** the latest row of each course that was last seen at most 30 s ago,
  like `query --max-age-secs 30`. They are drawn as SVG shapes over the base map
  image, so the map is sharp at any size.
- **Speed:** each vehicle's speed over the last 60 s, computed like the `KM/H` column of
  `query`: the straight-line distance between its positions over the time covered.
  `–` means there is not enough data yet, for example right after it appeared.
- **Hovering:** the page puts the SVG straight into the HTML, next to a JSON block with
  each vehicle's pixel position, line, speed and color. `src/map_hover.js` finds the
  vehicle nearest to the mouse (within 24 px), circles it and shows its line and speed.
  A tap works the same on touch screens. The plain `/map.svg` has a `<title>` on each
  vehicle instead, which browsers show as a tooltip.

`/map.svg` takes the same vehicle type and line prefixes as the pages, like
`/tram/map.svg` or `/bus/136/map.svg`. Its `<image>` points to `/map/wroclaw.jpg` on the
same server.

## Hovering the charts

The page puts each chart's SVG straight into the HTML (an SVG loaded with `<img>`
can't react to the mouse). Next to it is a JSON block with, for every chart step, its
x pixel, its time and, per line, the y pixel and the value as text. The server
formats the values the same way as the tables.

A small script (`src/chart_hover.js`, no libraries) reads that block. On hover it
snaps to the nearest step, draws a vertical line and a dot on each line, and shows
the time and all values. Without JavaScript the charts still show, just without
this. The charts can also be focused with Tab: Left and Right move between steps,
Escape hides the values.

The page has one table for all vehicles and one per vehicle type. A type gets a table
only when it had at least one vehicle in the last month, so trains are usually not
shown. Charts show one line per type that had vehicles in the chart's range. The
columns are the last hour, day, week and month (30 days), counted back from the
current minute.

## How the numbers are computed

All numbers are built from `vehicle_positions` rows (see
[database.md](database.md#vehicle_positions)).

- **Fetch times** are all `first_seen_unix` and last seen times in the range. Only
  times when the collector saved data count, so gaps when it was stopped don't lower
  the averages. A range with no fetch times shows `–`.
- **Average active vehicles:** at each fetch time, the rows that cover it are counted.
  This is the number of vehicles in the feed at that moment. The value is the mean
  over all fetch times in the range.
- **Peak active vehicles:** the largest of those counts.
- **Average speed:** total distance divided by total time. Two rows that follow each
  other on the same line and course are one step: the straight-line distance between
  the two positions, over the time from the first sighting at one position to the
  first sighting at the next. Time spent standing at stops counts, like the `KM/H`
  column of `query`. A vehicle usually gets a new GPS position only every 15 s, so a
  row "seen for" a few seconds is often a moving vehicle with an old position, not a
  stop.
  - Each step's time and distance are spread evenly over the minutes it covers. A
    10-minute stop adds a little standing time to each of those 10 minutes instead of
    all of it to the last one.
  - A step is skipped when the next row starts more than 60 s after the previous one
    was last seen, or when it would mean more than 120 km/h (a GPS jump).
  - A row without a usable step counts as standing time only when nothing at all was
    fetched in the 60 s after it: the collector was stopped, or it is the newest
    data. Without that, parked vehicles would drop out of the minutes before a restart
    and the speed would jump. When other vehicles were still fetched, the vehicle
    itself went missing, often during a layover at the terminus, and its time is left
    out, as is the last row of a course.
- **Distance driven:** the sum of those step distances, in km.

## Caching

The server keeps totals in memory in two caches:

| Cache | Resolution | Keeps | Used by |
|---|---|---|---|
| Recent | 1 minute | the last 30 days, totals per vehicle type only | the overview |
| Recent lines | 1 minute | the last day, also totals per line | the last hour and day of a line |
| History | 15 minutes | everything, also totals per line: about 60 MB per year of data | calendar pages; the last week and month of a line |

Totals per line are what takes the memory: about 65 lines run at a time, and each
keeps 24 bytes per bucket. To keep this small, a line's last week and last month come
from the history cache, so they start at the quarter hour before the exact time. For
all vehicles and vehicle types they start at the minute.

A step can reach up to 11 minutes back, so a bucket is kept once it ended at least 12
minutes ago; later buckets are computed again on each request. At start the server
computes the last 30 days, the last day with lines, and then all history in the
background, in 6-hour chunks. Each chunk is one query on the shared connection, so the
collector waits at most for one chunk. A year of data takes a few minutes; until it is
done, calendar pages and the last week and month of a line wait for it. Nothing is
stored in the database, so this happens after every restart.

## HTTP caching

Nothing is stored on the server, but the responses tell browsers how long they can
keep them:

| Response | `Cache-Control` | `ETag` |
|---|---|---|
| Calendar and data quality pages of a period whose data is final | `max-age=86400` | `"<git commit>-<language>"` |
| The base map, `/map/wroclaw.jpg` | `max-age=86400` | none |
| Everything else: overview, `?range=…`, `/quality`, chart and map images, periods not ended yet | `max-age=60` | none |

A period's data is final 12 minutes after it ends, like the finished buckets above.
When a request's `If-None-Match` has the page's `ETag` (or `*`), the server answers
`304` before computing anything, so it doesn't wait for the history cache either.

The `ETag` changes with each deploy, so new HTML is sent after it. A build from a dirty
working copy (usual with jj) or without git information has no commit to go by, so it
sends no `ETag` and every page gets `max-age=60`. Data imported for an old period later
shows only after the browser's copy expires.

## Tests

- `src/stats.rs`, `src/charts.rs`, `src/map.rs`, `src/web.rs`, `src/web/selection.rs`:
  unit tests with small made-up data. The map tests use a blank base map instead of
  downloaded tiles.
- `tests/live_backup.rs`: the overview's last-day values, a calendar day page and the
  day page of the busiest line all match a separate scan of the test backup, the
  lines' totals add up to their vehicle type's, and a real `collect` run with MPK
  unreachable shows 100% failed line-list downloads. `collect` serves the page and stops on `SIGTERM`, and `--web disabled`
  doesn't listen. These tests send MPK requests to a closed local proxy port, so they
  don't reach the internet.
- `tests/web_e2e.rs`: end-to-end tests in headless Chrome with `thirtyfour`, including
  moving the mouse over chart steps, using the arrow keys, going through the calendar
  pages, the data quality pages and their charts, and a browser set to Polish getting
  Polish pages. They start the server with the map disabled, so they don't download
  tiles and don't cover the map's hover script. Chrome's languages are set for each test, so the system language
  doesn't matter. They need
  Google Chrome. The matching chromedriver is downloaded on the first run; set
  `CHROMEDRIVER` (and `CHROME` for the browser) to use your own binaries instead, as
  the CI does.
