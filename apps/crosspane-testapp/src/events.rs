//! JSON-lines output intentionally includes text: this is the input test fixture.

use std::{
    collections::BTreeMap,
    fs::File,
    io::{self, BufWriter, Write},
    path::Path,
    time::Instant,
};

use anyhow::{Context, Result};
use crosspane_types::hid::MouseButton as HidButton;
use serde::Serialize;
use winit::event::{ElementState, MouseButton};

#[derive(Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub(crate) enum Event<'a> {
    Ready {
        scale: f64,
        size_px: [u32; 2],
    },
    Key {
        code: &'a str,
        state: &'static str,
        repeat: bool,
        text: Option<&'a str>,
    },
    ImeCommit {
        text: &'a str,
    },
    Pointer {
        x_px: f64,
        y_px: f64,
    },
    Button {
        button: u8,
        state: &'static str,
    },
    #[serde(rename = "wheel")]
    WheelLines {
        lines_x: f64,
        lines_y: f64,
    },
    #[serde(rename = "wheel")]
    WheelPixels {
        px_x: f64,
        px_y: f64,
    },
    Focus {
        focused: bool,
    },
    Resize {
        size_px: [u32; 2],
    },
    Scale {
        scale: f64,
    },
    Close,
}

#[derive(Serialize)]
struct Record<'a> {
    t_ns: u64,
    #[serde(flatten)]
    event: Event<'a>,
}

pub(crate) struct EventLog {
    start: Instant,
    writer: BufWriter<Box<dyn Write>>,
    other_buttons: BTreeMap<u16, u8>,
}

impl EventLog {
    pub(crate) fn new(path: Option<&Path>, start: Instant) -> Result<Self> {
        let writer: Box<dyn Write> = match path {
            Some(path) => Box::new(File::create(path).context("creating fixture event log")?),
            None => Box::new(io::stdout()),
        };
        Ok(Self {
            start,
            writer: BufWriter::new(writer),
            other_buttons: BTreeMap::new(),
        })
    }

    pub(crate) fn write(&mut self, event: Event<'_>) -> Result<()> {
        let record = Record {
            t_ns: u64::try_from(self.start.elapsed().as_nanos()).unwrap_or(u64::MAX),
            event,
        };
        serde_json::to_writer(&mut self.writer, &record).context("serializing fixture event")?;
        self.writer.write_all(b"\n")?;
        // Platform tests must see each event immediately, even when output is redirected.
        self.writer.flush().context("flushing fixture event log")
    }

    pub(crate) fn button(&mut self, button: MouseButton) -> Result<u8> {
        Ok(match button {
            MouseButton::Left => HidButton::PRIMARY.0,
            MouseButton::Right => HidButton::SECONDARY.0,
            MouseButton::Middle => HidButton::TERTIARY.0,
            MouseButton::Back => HidButton::BACK.0,
            MouseButton::Forward => HidButton::FORWARD.0,
            MouseButton::Other(raw) => {
                // Winit's other-button numbers are platform-specific (including evdev codes).
                // Assign stable, distinct HID numbers in this app's event stream, starting at 6.
                if let Some(&number) = self.other_buttons.get(&raw) {
                    number
                } else {
                    let number = u8::try_from(self.other_buttons.len() + 6)
                        .context("too many distinct mouse buttons for HID u8 numbering")?;
                    self.other_buttons.insert(raw, number);
                    number
                }
            }
        })
    }
}

pub(crate) fn state_name(state: ElementState) -> &'static str {
    match state {
        ElementState::Pressed => "pressed",
        ElementState::Released => "released",
    }
}
