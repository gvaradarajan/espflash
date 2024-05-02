//! Serial monitor utility
//!
//! While simple, this serial monitor does provide some nice features such as:
//!
//! - Keyboard shortcut for resetting the device (Ctrl-R)
//! - Decoding of function addresses in serial output
//!
//! While some serial monitors buffer output until a newline is encountered,
//! that is not the case here. With other monitors the output of a `print!()`
//! call are not displayed until `println!()` is subsequently called, where as
//! in our monitor the output is displayed immediately upon reading.

use regex::Regex;
use std::{
    fs::{File, OpenOptions},
    io::{stdout, BufWriter, ErrorKind, Read, Write},
    time::Duration,
};

use chrono::Local;

use crossterm::{
    event::{poll, read, Event, KeyCode, KeyEvent, KeyModifiers},
    terminal::{disable_raw_mode, enable_raw_mode},
};
use log::error;
use miette::{IntoDiagnostic, Result};
#[cfg(feature = "serialport")]
use serialport::SerialPort;
use strum::{Display, EnumIter, EnumString, VariantNames};

use crate::{
    cli::monitor::parser::{InputParser, ResolvingPrinter},
    connection::{reset::reset_after_flash, Port},
};

pub mod parser;

mod line_endings;
mod symbols;

#[cfg_attr(feature = "cli", derive(clap::ValueEnum))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Display, EnumIter, EnumString, VariantNames)]
#[non_exhaustive]
#[strum(serialize_all = "lowercase")]
pub enum LogFormat {
    /// defmt
    Defmt,
    /// serial
    Serial,
}

/// Type that ensures that raw mode is disabled when dropped.
struct RawModeGuard;

impl RawModeGuard {
    pub fn new() -> Result<Self> {
        enable_raw_mode().into_diagnostic()?;
        Ok(RawModeGuard)
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        if let Err(e) = disable_raw_mode() {
            error!("Failed to disable raw_mode: {:#}", e)
        }
    }
}

/// Open a serial monitor on the given serial port, using the given input parser.
pub fn monitor(
    mut serial: Port,
    elf: Option<&[u8]>,
    pid: u16,
    baud: u32,
    log_format: LogFormat,
    log_path: Option<String>,
    interactive_mode: bool,
) -> miette::Result<()> {
    if interactive_mode {
        println!("Commands:");
        println!("    CTRL+R    Reset chip");
        println!("    CTRL+C    Exit");
        println!();
    } else {
        reset_after_flash(&mut serial, pid).into_diagnostic()?;
    }

    // Explicitly set the baud rate when starting the serial monitor, to allow using
    // different rates for flashing.
    serial.set_baud_rate(baud).into_diagnostic()?;
    serial
        .set_timeout(Duration::from_millis(5))
        .into_diagnostic()?;

    // We are in raw mode until `_raw_mode` is dropped (ie. this function returns).
    let _raw_mode = RawModeGuard::new();

    let stdout = stdout();
    let mut stdout = ResolvingPrinter::new(elf, stdout.lock());

    let mut parser: Box<dyn InputParser> = match log_format {
        LogFormat::Defmt => Box::new(parser::esp_defmt::EspDefmt::new(elf)?),
        LogFormat::Serial => Box::new(parser::serial::Serial),
    };

    let mut buff = [0; 1024];
    let mut log_file: Option<BufWriter<File>> = if let Some(log_path) = log_path.as_ref() {
        let log_file_obj = OpenOptions::new().create(true).append(true).open(log_path);
        if let Err(err) = log_file_obj.as_ref() {
            println!("error opening log_file: {:?}", err);
        }
        log_file_obj.map(BufWriter::new).ok()
    } else {
        None
    };

    loop {
        let read_count = match serial.read(&mut buff) {
            Ok(count) => {
                if let Some(log_file) = log_file.as_mut() {
                    let line = String::from_utf8(buff.to_vec()).unwrap();
                    if let Err(err) = log_file
                        .write_all(strip_ansi_formatting_and_apply_timestamp(&line).as_bytes())
                    {
                        println!("could not write line {} to log file: {}", line, err);
                    }
                    log_file.write_all(b"\n").ok();
                }
                Ok(count)
            }
            Err(e) if e.kind() == ErrorKind::TimedOut => Ok(0),
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            err => err.into_diagnostic(),
        }?;

        parser.feed(&buff[0..read_count], &mut stdout);

        // Don't forget to flush the writer!
        if let Some(log_file) = log_file.as_mut() {
            log_file.flush().ok();
        }
        stdout.flush().ok();

        if interactive_mode && poll(Duration::from_secs(0)).into_diagnostic()? {
            if let Event::Key(key) = read().into_diagnostic()? {
                if key.modifiers.contains(KeyModifiers::CONTROL) {
                    match key.code {
                        KeyCode::Char('c') => break,
                        KeyCode::Char('r') => {
                            reset_after_flash(&mut serial, pid).into_diagnostic()?;
                            continue;
                        }
                        _ => {}
                    }
                }

                if let Some(bytes) = handle_key_event(key) {
                    serial.write_all(&bytes).into_diagnostic()?;
                    serial.flush().into_diagnostic()?;
                }
            }
        }
    }

    Ok(())
}

fn strip_ansi_formatting_and_apply_timestamp(line_str: &str) -> String {
    let re = Regex::new(r"\x1b\[[0-9;]*m").unwrap();
    let line_str = re.replace_all(line_str, "").to_string();
    let re = Regex::new(r";[0-9]*m").unwrap();
    let line_str = re.replace_all(&line_str, "").to_string();
    let current_time = Local::now().format("%+");
    format!("{current_time} - {line_str}")
}

// Converts key events from crossterm into appropriate character/escape
// sequences which are then sent over the serial connection.
//
// Adapted from: https://github.com/dhylands/serial-monitor
fn handle_key_event(key_event: KeyEvent) -> Option<Vec<u8>> {
    // The following escape sequences come from the MicroPython codebase.
    //
    //  Up      ESC [A
    //  Down    ESC [B
    //  Right   ESC [C
    //  Left    ESC [D
    //  Home    ESC [H  or ESC [1~
    //  End     ESC [F  or ESC [4~
    //  Del     ESC [3~
    //  Insert  ESC [2~

    let mut buf = [0; 4];

    let key_str: Option<&[u8]> = match key_event.code {
        KeyCode::Backspace => Some(b"\x08"),
        KeyCode::Enter => Some(b"\r"),
        KeyCode::Left => Some(b"\x1b[D"),
        KeyCode::Right => Some(b"\x1b[C"),
        KeyCode::Home => Some(b"\x1b[H"),
        KeyCode::End => Some(b"\x1b[F"),
        KeyCode::Up => Some(b"\x1b[A"),
        KeyCode::Down => Some(b"\x1b[B"),
        KeyCode::Tab => Some(b"\x09"),
        KeyCode::Delete => Some(b"\x1b[3~"),
        KeyCode::Insert => Some(b"\x1b[2~"),
        KeyCode::Esc => Some(b"\x1b"),
        KeyCode::Char(ch) => {
            if key_event.modifiers & KeyModifiers::CONTROL == KeyModifiers::CONTROL {
                buf[0] = ch as u8;

                if ch.is_ascii_lowercase() || (ch == ' ') {
                    buf[0] &= 0x1f;
                    Some(&buf[0..1])
                } else if ('4'..='7').contains(&ch) {
                    // crossterm returns Control-4 thru 7 for \x1c thru \x1f
                    buf[0] = (buf[0] + 8) & 0x1f;
                    Some(&buf[0..1])
                } else {
                    Some(ch.encode_utf8(&mut buf).as_bytes())
                }
            } else {
                Some(ch.encode_utf8(&mut buf).as_bytes())
            }
        }
        _ => None,
    };

    key_str.map(|slice| slice.into())
}
