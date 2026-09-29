use std::f64::consts::PI;

use anyhow::{Context, Result, anyhow};
use jpeg_encoder::{ColorType, Encoder};
use tiny_skia::{Color, ColorU8, Pixmap, PixmapPaint, Transform};

use crate::geo::MicroDegrees;

const TILE_URL: &str = "https://tile.openstreetmap.org";
const TILE_EXTENSION: &str = "png";
const ZOOM: u32 = 12;
const TILE_SIZE: u32 = 256;
const SOUTH: MicroDegrees = MicroDegrees(51_005_000);
const NORTH: MicroDegrees = MicroDegrees(51_210_000);
const WEST: MicroDegrees = MicroDegrees(16_815_000);
const EAST: MicroDegrees = MicroDegrees(17_185_000);
const HALF_TURN_DEGREES: f64 = 180.0;
const FULL_TURN_DEGREES: f64 = 360.0;
const MAX_CHANNEL: f32 = 255.0;
const RED_LUMA: f32 = 0.299;
const GREEN_LUMA: f32 = 0.587;
const BLUE_LUMA: f32 = 0.114;
const KEPT_SATURATION: f32 = 0.3;
const KEPT_CONTRAST: f32 = 0.5;
const JPEG_QUALITY: u8 = 85;
const MARKER_RADIUS: f64 = 6.0;
const SQUARE_HALF_SIDE: f64 = 5.3;
const TRIANGLE_TOP: f64 = 7.0;
const TRIANGLE_BOTTOM: f64 = 5.0;
const TRIANGLE_HALF_WIDTH: f64 = 6.5;
const OUTLINE_WIDTH: f64 = 1.5;
const OUTLINE_COLOR: &str = "#ffffff";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Symbol {
    Circle,
    Square,
    Triangle,
}

impl Symbol {
    #[must_use]
    pub fn svg(self, x: f64, y: f64, fill: &str) -> String {
        let paint =
            format!("fill=\"{fill}\" stroke=\"{OUTLINE_COLOR}\" stroke-width=\"{OUTLINE_WIDTH}\"");
        match self {
            Symbol::Circle => {
                format!("<circle cx=\"{x:.1}\" cy=\"{y:.1}\" r=\"{MARKER_RADIUS}\" {paint}/>")
            }
            Symbol::Square => format!(
                "<rect x=\"{:.1}\" y=\"{:.1}\" width=\"{}\" height=\"{}\" {paint}/>",
                x - SQUARE_HALF_SIDE,
                y - SQUARE_HALF_SIDE,
                2.0 * SQUARE_HALF_SIDE,
                2.0 * SQUARE_HALF_SIDE
            ),
            Symbol::Triangle => format!(
                "<polygon points=\"{:.1},{:.1} {:.1},{:.1} {:.1},{:.1}\" {paint}/>",
                x,
                y - TRIANGLE_TOP,
                x + TRIANGLE_HALF_WIDTH,
                y + TRIANGLE_BOTTOM,
                x - TRIANGLE_HALF_WIDTH,
                y + TRIANGLE_BOTTOM
            ),
        }
    }

    #[must_use]
    pub fn extent() -> f64 {
        TRIANGLE_TOP + OUTLINE_WIDTH
    }
}

#[derive(Clone)]
pub struct TileClient {
    http: reqwest::Client,
}

impl TileClient {
    #[must_use]
    pub fn new(http: reqwest::Client) -> Self {
        Self { http }
    }

    async fn tile(&self, x: u32, y: u32) -> Result<Pixmap> {
        let url = format!("{TILE_URL}/{ZOOM}/{x}/{y}.{TILE_EXTENSION}");
        let body = self
            .http
            .get(&url)
            .send()
            .await?
            .error_for_status()?
            .bytes()
            .await?;
        Pixmap::decode_png(&body).with_context(|| format!("invalid tile {url}"))
    }
}

struct Tile {
    x: u32,
    y: u32,
    pixmap: Pixmap,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Area {
    left: u32,
    top: u32,
    width: u32,
    height: u32,
}

impl Area {
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    fn of_bounds() -> Self {
        let (west, north) = world_pixel(NORTH, WEST);
        let (east, south) = world_pixel(SOUTH, EAST);
        let left = west.floor() as u32;
        let top = north.floor() as u32;
        Self {
            left,
            top,
            width: east.ceil() as u32 - left,
            height: south.ceil() as u32 - top,
        }
    }

    fn tiles(self) -> impl Iterator<Item = (u32, u32)> {
        let columns = self.left / TILE_SIZE..=(self.left + self.width - 1) / TILE_SIZE;
        let rows = self.top / TILE_SIZE..=(self.top + self.height - 1) / TILE_SIZE;
        columns.flat_map(move |x| rows.clone().map(move |y| (x, y)))
    }
}

fn world_pixel(latitude: MicroDegrees, longitude: MicroDegrees) -> (f64, f64) {
    let size = f64::from(TILE_SIZE << ZOOM);
    let x = (longitude.degrees() + HALF_TURN_DEGREES) / FULL_TURN_DEGREES * size;
    let y = (1.0 - latitude.degrees().to_radians().tan().asinh() / PI) / 2.0 * size;
    (x, y)
}

#[derive(Clone)]
pub struct Basemap {
    area: Area,
    jpeg: Vec<u8>,
}

impl Basemap {
    pub async fn download(client: &TileClient) -> Result<Self> {
        let area = Area::of_bounds();
        let mut tiles = Vec::new();
        for (x, y) in area.tiles() {
            tiles.push(Tile {
                x,
                y,
                pixmap: client.tile(x, y).await?,
            });
        }
        Self::encode(area, &stitch(area, &tiles)?)
    }

    fn encode(area: Area, pixmap: &Pixmap) -> Result<Self> {
        let mut jpeg = Vec::new();
        Encoder::new(&mut jpeg, JPEG_QUALITY).encode(
            pixmap.data(),
            u16::try_from(area.width)?,
            u16::try_from(area.height)?,
            ColorType::Rgba,
        )?;
        Ok(Self { area, jpeg })
    }

    #[must_use]
    pub fn width(&self) -> u32 {
        self.area.width
    }

    #[must_use]
    pub fn height(&self) -> u32 {
        self.area.height
    }

    #[must_use]
    pub fn jpeg(&self) -> &[u8] {
        &self.jpeg
    }

    #[must_use]
    pub fn pixel(&self, latitude: MicroDegrees, longitude: MicroDegrees) -> Option<(f64, f64)> {
        let (x, y) = self.unclipped_pixel(latitude, longitude);
        let inside = (0.0..f64::from(self.area.width)).contains(&x)
            && (0.0..f64::from(self.area.height)).contains(&y);
        inside.then_some((x, y))
    }

    /// Like [`Basemap::pixel`], but also for points outside the map.
    #[must_use]
    pub fn unclipped_pixel(&self, latitude: MicroDegrees, longitude: MicroDegrees) -> (f64, f64) {
        let (x, y) = world_pixel(latitude, longitude);
        (x - f64::from(self.area.left), y - f64::from(self.area.top))
    }
}

#[cfg(test)]
impl Basemap {
    pub(crate) fn empty() -> Self {
        let area = Area::of_bounds();
        Self::encode(area, &stitch(area, &[]).unwrap()).unwrap()
    }
}

#[allow(clippy::cast_possible_wrap)]
fn stitch(area: Area, tiles: &[Tile]) -> Result<Pixmap> {
    let mut pixmap = Pixmap::new(area.width, area.height)
        .ok_or_else(|| anyhow!("invalid map size {}x{}", area.width, area.height))?;
    pixmap.fill(Color::WHITE);
    for tile in tiles {
        pixmap.draw_pixmap(
            (tile.x * TILE_SIZE) as i32 - area.left as i32,
            (tile.y * TILE_SIZE) as i32 - area.top as i32,
            tile.pixmap.as_ref(),
            &PixmapPaint::default(),
            Transform::identity(),
            None,
        );
    }
    fade(&mut pixmap);
    Ok(pixmap)
}

fn fade(pixmap: &mut Pixmap) {
    for pixel in pixmap.pixels_mut() {
        let color = pixel.demultiply();
        let gray = RED_LUMA * f32::from(color.red())
            + GREEN_LUMA * f32::from(color.green())
            + BLUE_LUMA * f32::from(color.blue());
        let channel = |value: u8| {
            let colored = gray + (f32::from(value) - gray) * KEPT_SATURATION;
            to_channel(MAX_CHANNEL - (MAX_CHANNEL - colored) * KEPT_CONTRAST)
        };
        *pixel = ColorU8::from_rgba(
            channel(color.red()),
            channel(color.green()),
            channel(color.blue()),
            u8::MAX,
        )
        .premultiply();
    }
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn to_channel(value: f32) -> u8 {
    value.round().clamp(0.0, MAX_CHANNEL) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    const BLACK: u8 = 0;
    const WHITE: u8 = u8::MAX;
    const JPEG_START: [u8; 2] = [0xff, 0xd8];
    const CITY_CENTER: (MicroDegrees, MicroDegrees) =
        (MicroDegrees(51_110_000), MicroDegrees(17_030_000));
    const BERLIN: (MicroDegrees, MicroDegrees) =
        (MicroDegrees(52_520_000), MicroDegrees(13_405_000));

    #[test]
    fn projects_like_openstreetmap_tiles() {
        let (x, y) = world_pixel(CITY_CENTER.0, CITY_CENTER.1);
        let tile_size = f64::from(TILE_SIZE);
        assert_eq!(
            ((x / tile_size).floor(), (y / tile_size).floor()),
            (2241.0, 1369.0)
        );
    }

    #[test]
    fn covers_the_area_with_whole_tiles() {
        let area = Area::of_bounds();
        let tiles: Vec<(u32, u32)> = area.tiles().collect();
        let (first_x, first_y) = tiles[0];
        let (last_x, last_y) = tiles[tiles.len() - 1];
        assert!(first_x * TILE_SIZE <= area.left && first_y * TILE_SIZE <= area.top);
        assert!((last_x + 1) * TILE_SIZE >= area.left + area.width);
        assert!((last_y + 1) * TILE_SIZE >= area.top + area.height);
        let columns = last_x - first_x + 1;
        let rows = last_y - first_y + 1;
        assert_eq!(tiles.len(), (columns * rows) as usize);
    }

    #[test]
    fn stitches_tiles_at_their_place_and_fades_them() {
        let area = Area::of_bounds();
        let (x, y) = area.tiles().nth(1).unwrap();
        let mut black = Pixmap::new(TILE_SIZE, TILE_SIZE).unwrap();
        black.fill(Color::BLACK);
        let image = stitch(
            area,
            &[Tile {
                x,
                y,
                pixmap: black,
            }],
        )
        .unwrap();
        assert_eq!((image.width(), image.height()), (area.width, area.height));
        let inside = image
            .pixel(
                x * TILE_SIZE + TILE_SIZE / 2 - area.left,
                y * TILE_SIZE + TILE_SIZE / 2 - area.top,
            )
            .unwrap()
            .demultiply();
        assert!(inside.red() > BLACK && inside.red() < WHITE, "{inside:?}");
        assert_eq!(image.pixel(0, 0).unwrap().demultiply().red(), WHITE);
    }

    #[test]
    fn encodes_the_map_as_jpeg() {
        assert!(Basemap::empty().jpeg().starts_with(&JPEG_START));
    }

    #[test]
    fn places_only_points_inside_the_map() {
        let basemap = Basemap::empty();
        let (x, y) = basemap.pixel(CITY_CENTER.0, CITY_CENTER.1).unwrap();
        assert!(x > 0.0 && x < f64::from(basemap.width()));
        assert!(y > 0.0 && y < f64::from(basemap.height()));
        assert_eq!(basemap.pixel(BERLIN.0, BERLIN.1), None);
        let (north_x, north_y) = basemap
            .pixel(MicroDegrees(CITY_CENTER.0.0 + 10_000), CITY_CENTER.1)
            .unwrap();
        assert!((north_x - x).abs() < f64::EPSILON && north_y < y);
    }
}
