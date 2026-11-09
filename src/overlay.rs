use std::{
    fs::File,
    io::Read,
    path::PathBuf,
    sync::{Arc, Mutex},
    thread,
};

use serde::{Deserialize, Serialize};
use tiny_http::{Header, Method, Request, Response, Server};

use crate::{
    replay::SharedHistory,
    stats::{Stats, save_json},
};

const ADDRESS: &str = "127.0.0.1:17333";
pub const URL: &str = "http://127.0.0.1:17333/";
pub const EDITOR_URL: &str = "http://127.0.0.1:17333/editor";
pub const TREASURES_URL: &str = "http://127.0.0.1:17333/treasures";
const MAX_BODY: usize = 8192;

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum Layout {
    Cards,
    Text,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum Alignment {
    Left,
    Center,
    Right,
}

#[derive(Deserialize, Serialize)]
#[serde(default)]
struct Typography {
    weight: u16,
    alignment: Alignment,
    stroke_width: u8,
    stroke_color: String,
    shadow_blur: u8,
    shadow_color: String,
}

impl Default for Typography {
    fn default() -> Self {
        Self {
            weight: 600,
            alignment: Alignment::Left,
            stroke_width: 0,
            stroke_color: "#000000".into(),
            shadow_blur: 0,
            shadow_color: "#000000".into(),
        }
    }
}

#[derive(Deserialize, Serialize)]
struct Appearance {
    layout: Layout,
    template: String,
    font: String,
    size: u8,
    text_color: String,
    accent_color: String,
    background_color: String,
    background_opacity: u8,
    #[serde(default)]
    typography: Typography,
}

impl Default for Appearance {
    fn default() -> Self {
        Self {
            layout: Layout::Cards,
            template: "Chests this match: {match}\nAll time: {total}\nAverage: {average}".into(),
            font: "system-ui".into(),
            size: 32,
            text_color: "#ffffff".into(),
            accent_color: "#ffd464".into(),
            background_color: "#101722".into(),
            background_opacity: 93,
            typography: Typography::default(),
        }
    }
}

impl Appearance {
    fn valid(&self) -> bool {
        let is_color = |color: &str| {
            color.len() == 7
                && color.starts_with('#')
                && color.as_bytes()[1..].iter().all(u8::is_ascii_hexdigit)
        };
        self.template.chars().count() <= 1000
            && matches!(
                self.font.as_str(),
                "system-ui" | "Arial" | "Segoe UI" | "Georgia" | "Consolas" | "Impact"
            )
            && (12..=128).contains(&self.size)
            && is_color(&self.text_color)
            && is_color(&self.accent_color)
            && is_color(&self.background_color)
            && self.background_opacity <= 100
            && matches!(self.typography.weight, 400 | 600 | 700 | 900)
            && self.typography.stroke_width <= 8
            && is_color(&self.typography.stroke_color)
            && self.typography.shadow_blur <= 24
            && is_color(&self.typography.shadow_color)
    }
}

pub fn start(
    stats: Arc<Mutex<Stats>>,
    path: PathBuf,
    history: SharedHistory,
) -> Result<(), String> {
    let server = Server::http(ADDRESS).map_err(|error| error.to_string())?;
    let mut appearance = File::open(&path)
        .ok()
        .and_then(|file| serde_json::from_reader::<_, Appearance>(file).ok())
        .filter(Appearance::valid)
        .unwrap_or_default();

    thread::Builder::new()
        .name("obs-overlay".into())
        .spawn(move || {
            for mut request in server.incoming_requests() {
                let host = header(&request, "Host");
                let origin = match host {
                    Some(ADDRESS) => "http://127.0.0.1:17333",
                    Some("localhost:17333") => "http://localhost:17333",
                    _ => {
                        respond(
                            request,
                            403,
                            "text/plain",
                            "Use the local overlay URL".into(),
                        );
                        continue;
                    }
                };
                let (status, content_type, body) = match (request.method(), request.url()) {
                    (&Method::Get, "/" | "/editor") => {
                        (200, "text/html", include_str!("../overlay.html").into())
                    }
                    (&Method::Get, "/treasures") => {
                        (200, "text/html", include_str!("../treasures.html").into())
                    }
                    (&Method::Get, "/treasure-data") => (
                        200,
                        "application/json",
                        serde_json::to_string(&*history.lock().unwrap()).unwrap(),
                    ),
                    (&Method::Get, "/stats") => {
                        let stats = *stats.lock().unwrap();
                        let body = serde_json::json!({
                            "current": stats.current.to_string(),
                            "all_time": stats.all_time.to_string(),
                            "average": stats.average_label(),
                            "appearance": appearance,
                        });
                        (200, "application/json", body.to_string())
                    }
                    (&Method::Get, "/appearance/defaults") => (
                        200,
                        "application/json",
                        serde_json::to_string(&Appearance::default()).unwrap(),
                    ),
                    (&Method::Post, "/appearance") => {
                        let (status, body) = if header(&request, "Origin") != Some(origin) {
                            (403, "Open the editor on this tracker to save changes")
                        } else if let Some(next) = read_appearance(&mut request) {
                            match save_json(&path, &next) {
                                Ok(()) => {
                                    appearance = next;
                                    (200, "Saved")
                                }
                                Err(error) => {
                                    eprintln!("Could not save OBS appearance: {error}");
                                    (500, "Could not save appearance")
                                }
                            }
                        } else {
                            (400, "Invalid appearance settings")
                        };
                        (status, "text/plain", body.into())
                    }
                    _ => (404, "text/plain", "Not found".into()),
                };
                respond(request, status, content_type, body);
            }
        })
        .map_err(|error| error.to_string())?;
    Ok(())
}

fn header<'a>(request: &'a Request, name: &'static str) -> Option<&'a str> {
    request
        .headers()
        .iter()
        .find(|header| header.field.equiv(name))
        .map(|header| header.value.as_str())
}

fn read_appearance(request: &mut Request) -> Option<Appearance> {
    let length = request.body_length()?;
    if length > MAX_BODY || header(request, "Content-Type") != Some("application/json") {
        return None;
    }
    let mut body = Vec::with_capacity(length);
    request
        .as_reader()
        .take(length as u64)
        .read_to_end(&mut body)
        .ok()?;
    serde_json::from_slice::<Appearance>(&body)
        .ok()
        .filter(Appearance::valid)
}

fn respond(request: Request, status: u16, content_type: &str, body: String) {
    let response = Response::from_string(body)
        .with_status_code(status)
        .with_header(
            Header::from_bytes("Content-Type", format!("{content_type}; charset=utf-8")).unwrap(),
        )
        .with_header(Header::from_bytes("Cache-Control", "no-store").unwrap())
        .with_header(Header::from_bytes("X-Frame-Options", "DENY").unwrap());
    let _ = request.respond(response);
}
