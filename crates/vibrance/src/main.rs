mod app;
mod columns;
mod config;
mod layout;
mod model;
mod persist;
mod queue;
mod theme;
mod ui;
mod view;

use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use iced::{Size, window};
use vibrance_audio::Sink;

use crate::app::{App, Options};
use crate::config::{Config, Paths, TitleBar};

const USAGE: &str = "\
vibrance [options]

  --audio-sink device|null|wav:<path>   where audio goes (default: device)
  --screenshot <path.png>               save a window screenshot, then exit
  --screenshot-after <ms>               delay before the screenshot (default 1500)
  --script <step;step;...>              run UI steps at startup (testing)

Set VIBRANCE_TRACE=1 to log the startup timeline, VIBRANCE_HOME=<dir> to
keep config, data, and cache under one directory.
";

/// Process start, for the startup timeline (SPEC 7.3).
pub static LAUNCH: OnceLock<Instant> = OnceLock::new();

fn main() -> iced::Result {
    let start = *LAUNCH.get_or_init(Instant::now);
    let default_filter = if std::env::var_os("VIBRANCE_TRACE").is_some() { "warn,lofty=error,vibrance=debug" } else { "warn,lofty=error,vibrance=info" };
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(default_filter))
        .format(move |buf, record| {
            use std::io::Write;
            writeln!(buf, "[{:>8.1}ms {} {}] {}", start.elapsed().as_secs_f64() * 1e3, record.level(), record.target(), record.args())
        })
        .init();

    let options = match parse_args() {
        Ok(options) => options,
        Err(message) => {
            eprintln!("{message}\n\n{USAGE}");
            std::process::exit(2);
        }
    };
    let decorations = Config::load(&Paths::resolve()).title_bar == TitleBar::System;

    let mut application = iced::application(move || App::new(options.clone()), App::update, App::view)
        .title(App::title)
        .subscription(App::subscription)
        .theme(App::iced_theme)
        .style(|app: &App, _| iced::theme::Style {
            background_color: if app.custom_title_bar() { iced::Color::TRANSPARENT } else { app.colors.surface },
            text_color: app.colors.on_surface,
        })
        .window(window::Settings {
            size: Size::new(1600.0, 1000.0),
            min_size: Some(Size::new(960.0, 600.0)),
            decorations,
            transparent: !decorations,
            exit_on_close_request: false,
            ..window::Settings::default()
        })
        .default_font(theme::FONT)
        .antialiasing(true);
    for font in theme::FONT_BYTES {
        application = application.font(font);
    }
    log::debug!("starting event loop");
    application.run()
}

impl App {
    fn iced_theme(&self) -> iced::Theme {
        iced::Theme::custom(
            "Vibrance",
            iced::theme::Palette {
                background: self.colors.surface,
                text: self.colors.on_surface,
                primary: self.colors.primary,
                success: self.colors.tertiary,
                warning: self.colors.tertiary,
                danger: self.colors.error,
            },
        )
    }
}

fn parse_args() -> Result<Options, String> {
    let mut options = Options { sink: Sink::Device, screenshot: None, screenshot_after: Duration::from_millis(1500), script: Vec::new() };
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut value = || args.next().ok_or_else(|| format!("{arg} needs a value"));
        match arg.as_str() {
            "--audio-sink" => {
                let v = value()?;
                options.sink = match v.as_str() {
                    "device" => Sink::Device,
                    "null" => Sink::Null,
                    _ => match v.strip_prefix("wav:") {
                        Some(path) => Sink::Wav { path: PathBuf::from(path), realtime: true },
                        None => return Err(format!("unknown audio sink: {v}")),
                    },
                };
            }
            "--screenshot" => options.screenshot = Some(PathBuf::from(value()?)),
            "--screenshot-after" => {
                options.screenshot_after = Duration::from_millis(value()?.parse().map_err(|_| "--screenshot-after takes milliseconds")?);
            }
            "--script" => options.script.extend(value()?.split(';').filter(|s| !s.is_empty()).map(str::to_owned)),
            "-h" | "--help" => {
                print!("{USAGE}");
                std::process::exit(0);
            }
            other => return Err(format!("unknown option: {other}")),
        }
    }
    Ok(options)
}
