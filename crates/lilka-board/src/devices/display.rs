use anyhow::{Result, bail, ensure};
use std::collections::{BTreeMap, VecDeque};

#[derive(Default, serde::Serialize)]
pub struct DisplayDiagnostics {
    pub ignored_commands: BTreeMap<u8, u64>,
    pub unknown_commands: BTreeMap<u8, u64>,
    pub ignored_data_bytes: BTreeMap<u8, u64>,
    pub journal: VecDeque<String>,
}
impl DisplayDiagnostics {
    fn command(&mut self, command: u8, known: bool) {
        let counts = if known {
            &mut self.ignored_commands
        } else {
            &mut self.unknown_commands
        };
        *counts.entry(command).or_default() += 1;
        if self.journal.len() == 128 {
            self.journal.pop_front();
        }
        self.journal.push_back(format!(
            "{} ST7789 command 0x{command:02x}",
            if known { "ignored" } else { "unknown" }
        ));
    }
}

pub const WIDTH: usize = 280;
pub const HEIGHT: usize = 240;

pub struct Display {
    strict: bool,
    diagnostics: DisplayDiagnostics,
    pixels: Vec<u16>,
    command: u8,
    parameters: Vec<u8>,
    column: (u16, u16),
    row: (u16, u16),
    cursor: (u16, u16),
    high_byte: Option<u8>,
    powered: bool,
    awake: bool,
    on: bool,
    bgr: bool,
    madctl: u8,
    panel: bool,
    pub generation: u64,
    pub pixels_written: usize,
    pub completed_large_windows: u64,
    pub changed_large_windows: u64,
    window_changed: bool,
}

impl Default for Display {
    fn default() -> Self {
        Self {
            strict: false,
            diagnostics: DisplayDiagnostics::default(),
            pixels: vec![0; WIDTH * HEIGHT],
            command: 0,
            parameters: Vec::new(),
            column: (0, WIDTH as u16 - 1),
            row: (0, HEIGHT as u16 - 1),
            cursor: (0, 0),
            high_byte: None,
            powered: false,
            awake: false,
            on: false,
            bgr: false,
            madctl: 0,
            panel: false,
            generation: 0,
            pixels_written: 0,
            completed_large_windows: 0,
            changed_large_windows: 0,
            window_changed: false,
        }
    }
}

impl Display {
    pub fn set_strict(&mut self, strict: bool) {
        self.strict = strict;
    }
    pub fn diagnostics(&self) -> &DisplayDiagnostics {
        &self.diagnostics
    }

    pub fn lilka() -> Self {
        Self {
            panel: true,
            ..Self::default()
        }
    }

    fn visible_position(&self, x: u16, y: u16) -> Option<(usize, usize)> {
        let (mut x, mut y) = (i32::from(x), i32::from(y));
        if self.panel {
            if self.madctl & 0x20 != 0 {
                std::mem::swap(&mut x, &mut y);
            }
            if self.madctl & 0x40 != 0 {
                x = 239 - x;
            }
            if self.madctl & 0x80 != 0 {
                y = 319 - y;
            }
            (x, y) = (299 - y, x);
        }
        ((0..WIDTH as i32).contains(&x) && (0..HEIGHT as i32).contains(&y))
            .then_some((x as usize, y as usize))
    }
    pub fn power(&mut self, powered: bool) {
        if self.powered != powered {
            self.powered = powered;
            self.generation += 1;
        }
    }
    fn write_row(&mut self, bytes: &[u8]) -> Option<usize> {
        if self.high_byte.is_some() || self.column.1 > 319 || self.row.1 > 319 {
            return None;
        }
        let remaining = self.column.1.checked_sub(self.cursor.0)? as usize + 1;
        let count = (bytes.len() / 2).min(remaining);
        if count < 8 {
            return None;
        }
        let (x, y) = self.cursor;
        let first = self.visible_position(x, y)?;
        let last = self.visible_position(x + count as u16 - 1, y)?;
        let begin = (first.1 * WIDTH + first.0) as isize;
        let end = (last.1 * WIDTH + last.0) as isize;
        let step = (end - begin) / (count as isize - 1);
        if !matches!(step, 1 | -1) && step.unsigned_abs() != WIDTH {
            return None;
        }
        let mut position = begin;
        let mut changed = false;
        for pair in bytes[..count * 2].chunks_exact(2) {
            let color = u16::from_be_bytes([pair[0], pair[1]]);
            let target = &mut self.pixels[position as usize];
            changed |= *target != color;
            *target = color;
            position += step;
        }
        self.window_changed |= changed;
        self.generation += count as u64;
        self.pixels_written += count;
        self.cursor.0 += count as u16;
        if count == remaining {
            self.cursor.0 = self.column.0;
            self.cursor.1 += 1;
            if self.cursor.1 > self.row.1 {
                self.cursor.1 = self.row.0;
                let area = (u64::from(self.column.1.saturating_sub(self.column.0)) + 1)
                    * (u64::from(self.row.1.saturating_sub(self.row.0)) + 1);
                if area >= (WIDTH * HEIGHT / 2) as u64 {
                    self.completed_large_windows += 1;
                    self.changed_large_windows += u64::from(self.window_changed);
                }
                self.window_changed = false;
            }
        }
        Some(count * 2)
    }
    pub fn write(&mut self, dc: bool, bytes: &[u8]) -> Result<()> {
        if !self.powered {
            return Ok(());
        }
        let mut bytes = bytes;
        if dc && self.command == 0x2c {
            while let Some(consumed) = self.write_row(bytes) {
                bytes = &bytes[consumed..];
            }
        }
        for &byte in bytes {
            if !dc {
                self.command = byte;
                self.parameters.clear();
                self.high_byte = None;
                match byte {
                    0x01 => {
                        let generation = self.generation + 1;
                        let panel = self.panel;
                        let strict = self.strict;
                        let diagnostics = std::mem::take(&mut self.diagnostics);
                        *self = Self::default();
                        self.strict = strict;
                        self.diagnostics = diagnostics;
                        self.panel = panel;
                        self.powered = true;
                        self.generation = generation;
                    }
                    0x10 => {
                        self.awake = false;
                        self.generation += 1;
                    }
                    0x11 => {
                        self.awake = true;
                        self.generation += 1;
                    }
                    0x28 => {
                        self.on = false;
                        self.generation += 1;
                    }
                    0x29 => {
                        self.on = true;
                        self.generation += 1;
                    }
                    0x34 | 0x35 => self.diagnostics.command(byte, true),
                    0x2c => {
                        self.cursor = (self.column.0, self.row.0);
                        self.window_changed = false;
                    }
                    0x2a | 0x2b | 0x36 | 0x3a => {}
                    0x00 | 0x13 | 0x20 | 0x21 | 0xb0 | 0xb2 | 0xb7 | 0xbb | 0xc0 | 0xc2 | 0xc3
                    | 0xc4 | 0xc6 | 0xd0 | 0xe0 | 0xe1 => self.diagnostics.command(byte, true),
                    _ => {
                        self.diagnostics.command(byte, false);
                        if self.strict {
                            bail!("ST7789 command {byte:02x} is not modeled");
                        }
                    }
                }
            } else if self.command == 0x2c {
                if let Some(high) = self.high_byte.take() {
                    let color = u16::from_be_bytes([high, byte]);
                    let (x, y) = self.cursor;
                    if let Some((x, y)) = self.visible_position(x, y) {
                        self.window_changed |= self.pixels[y * WIDTH + x] != color;
                        self.pixels[y * WIDTH + x] = color;
                        self.generation += 1;
                    }
                    self.pixels_written += 1;
                    self.cursor.0 += 1;
                    if self.cursor.0 > self.column.1 {
                        self.cursor.0 = self.column.0;
                        self.cursor.1 += 1;
                        if self.cursor.1 > self.row.1 {
                            self.cursor.1 = self.row.0;
                            let area = (u64::from(self.column.1.saturating_sub(self.column.0)) + 1)
                                * (u64::from(self.row.1.saturating_sub(self.row.0)) + 1);
                            if area >= (WIDTH * HEIGHT / 2) as u64 {
                                self.completed_large_windows += 1;
                                self.changed_large_windows += u64::from(self.window_changed);
                            }
                            self.window_changed = false;
                        }
                    }
                } else {
                    self.high_byte = Some(byte);
                }
            } else {
                if matches!(self.command, 0x34 | 0x35)
                    || self
                        .diagnostics
                        .unknown_commands
                        .contains_key(&self.command)
                {
                    *self
                        .diagnostics
                        .ignored_data_bytes
                        .entry(self.command)
                        .or_default() += 1;
                    continue;
                }
                self.parameters.push(byte);
                match self.command {
                    0x2a | 0x2b if self.parameters.len() == 4 => {
                        let start = u16::from_be_bytes([self.parameters[0], self.parameters[1]]);
                        let end = u16::from_be_bytes([self.parameters[2], self.parameters[3]]);
                        ensure!(start <= end && end < 320, "Invalid ST7789 window");
                        if self.command == 0x2a {
                            self.column = (start, end);
                        } else {
                            self.row = (start, end);
                        }
                    }
                    0x3a => ensure!(
                        byte == 0x55 && self.parameters.len() == 1,
                        "Only ST7789 RGB565 is modeled"
                    ),
                    0x36 => {
                        ensure!(self.parameters.len() == 1, "Excess MADCTL data");
                        self.bgr = byte & 8 != 0;
                        self.madctl = byte;
                    }
                    0x2a | 0x2b => {
                        ensure!(self.parameters.len() < 4, "Excess ST7789 window parameters")
                    }
                    0xb0 | 0xb2 | 0xb7 | 0xbb | 0xc0 | 0xc2 | 0xc3 | 0xc4 | 0xc6 | 0xd0 | 0xe0
                    | 0xe1 => {
                        let count = match self.command {
                            0xb0 | 0xd0 => 2,
                            0xb2 => 5,
                            0xe0 | 0xe1 => 14,
                            _ => 1,
                        };
                        if self.parameters.len() > count {
                            *self
                                .diagnostics
                                .ignored_data_bytes
                                .entry(self.command)
                                .or_default() += 1;
                            self.parameters.truncate(count);
                            if self.strict {
                                bail!("Excess panel configuration data");
                            }
                        }
                    }
                    _ => {
                        *self
                            .diagnostics
                            .ignored_data_bytes
                            .entry(self.command)
                            .or_default() += 1;
                        self.parameters.clear();
                        if self.strict {
                            bail!("Unexpected data for ST7789 command {:02x}", self.command);
                        }
                    }
                }
            }
        }
        Ok(())
    }

    pub fn pixel(&self, x: usize, y: usize) -> u16 {
        self.pixels[y * WIDTH + x]
    }

    fn colors(&self) -> impl Iterator<Item = [u8; 3]> + '_ {
        let visible = self.powered && self.awake && self.on;
        self.pixels.iter().map(move |&value| {
            let value = if visible { value } else { 0 };
            let mut rgb = [
                (((value >> 11) & 31) * 255 / 31) as u8,
                (((value >> 5) & 63) * 255 / 63) as u8,
                ((value & 31) * 255 / 31) as u8,
            ];
            if self.bgr {
                rgb.swap(0, 2);
            }
            rgb
        })
    }

    pub fn rgb(&self) -> Vec<u8> {
        self.colors().flatten().collect()
    }

    pub fn snapshot_rgb565(&self, pixels: &mut Vec<u16>) -> (bool, bool) {
        pixels.clear();
        pixels.extend_from_slice(&self.pixels);
        (self.powered && self.awake && self.on, self.bgr)
    }

    pub fn rgba(&self) -> Vec<u8> {
        self.colors().flat_map(|[r, g, b]| [r, g, b, 255]).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bulk_rows_match_single_byte_writes_in_every_panel_orientation() -> Result<()> {
        for panel in [false, true] {
            for mode in [0, 0x20, 0x40, 0x60, 0x80, 0xa0, 0xc0, 0xe0] {
                let create = || {
                    let mut d = Display::default();
                    d.power(true);
                    d.panel = panel;
                    d.madctl = mode;
                    d.column = (20, 239);
                    d.row = (20, 199);
                    d.command = 0x2c;
                    d.cursor = (20, 20);
                    d
                };
                let mut bulk = create();
                let mut scalar = create();
                let bytes = (0..(220 * 180 * 4 + 1))
                    .map(|i| (i * 37) as u8)
                    .collect::<Vec<_>>();
                bulk.write(true, &bytes)?;
                for byte in &bytes {
                    scalar.write(true, &[*byte])?;
                }
                assert_eq!(bulk.pixels, scalar.pixels);
                assert_eq!(bulk.cursor, scalar.cursor);
                assert_eq!(bulk.high_byte, scalar.high_byte);
                assert_eq!(bulk.generation, scalar.generation);
                assert_eq!(bulk.pixels_written, scalar.pixels_written);
                assert_eq!(bulk.completed_large_windows, scalar.completed_large_windows);
                assert_eq!(bulk.changed_large_windows, scalar.changed_large_windows);
            }
        }
        Ok(())
    }
    #[test]
    fn split_parameters_and_pixel_bytes_survive_spi_boundaries() -> Result<()> {
        let mut display = Display::default();
        display.power(true);
        display.write(false, &[0x11, 0x29, 0x2a])?;
        display.write(true, &[0, 1])?;
        display.write(true, &[0, 2])?;
        display.write(false, &[0x2b])?;
        display.write(true, &[0, 3, 0, 3])?;
        display.write(false, &[0x2c])?;
        display.write(true, &[0xf8])?;
        display.write(true, &[0, 0x07, 0xe0])?;
        assert_eq!(display.pixel(1, 3), 0xf800);
        assert_eq!(display.pixel(2, 3), 0x07e0);
        assert_eq!(
            &display.rgb()[((3 * WIDTH + 1) * 3)..][..6],
            &[255, 0, 0, 0, 255, 0]
        );
        Ok(())
    }
    #[test]
    fn completed_windows_distinguish_duplicate_and_changed_frames() -> Result<()> {
        let mut display = Display::default();
        display.power(true);
        display.write(false, &[0x11, 0x29, 0x2a])?;
        display.write(true, &[0, 0, 1, 23])?;
        display.write(false, &[0x2b])?;
        display.write(true, &[0, 0, 0, 239])?;
        let frame = vec![0; WIDTH * HEIGHT * 2];
        for _ in 0..2 {
            display.write(false, &[0x2c])?;
            display.write(true, &frame)?;
        }
        assert_eq!(display.completed_large_windows, 2);
        assert_eq!(display.changed_large_windows, 0);
        display.write(false, &[0x2c])?;
        display.write(true, &[0xf8, 0])?;
        assert_eq!(display.completed_large_windows, 2);
        display.write(true, &frame[2..])?;
        assert_eq!(display.completed_large_windows, 3);
        assert_eq!(display.changed_large_windows, 1);
        Ok(())
    }
    #[test]
    fn rgba_preserves_colors_alpha_and_panel_visibility() {
        let mut display = Display::default();
        display.pixels[..3].copy_from_slice(&[0xf800, 0x07e0, 0x001f]);
        for (powered, awake, on, bgr) in [
            (true, true, true, false),
            (true, true, true, true),
            (false, true, true, false),
            (true, false, true, false),
            (true, true, false, false),
        ] {
            display.powered = powered;
            display.awake = awake;
            display.on = on;
            display.bgr = bgr;
            let rgba = display.rgba();
            let expected = if !powered || !awake || !on {
                [0, 0, 0, 255, 0, 0, 0, 255, 0, 0, 0, 255]
            } else if bgr {
                [0, 0, 255, 255, 0, 255, 0, 255, 255, 0, 0, 255]
            } else {
                [255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255]
            };
            assert_eq!(rgba.len(), WIDTH * HEIGHT * 4);
            assert_eq!(&rgba[..12], &expected);
            assert!(rgba.chunks_exact(4).all(|p| p[3] == 255));
        }
    }
}

#[cfg(test)]
mod compatibility_tests {
    use super::*;
    #[test]
    fn tearing_and_unknown_commands_preserve_frame_and_bound_diagnostics() -> Result<()> {
        let mut d = Display::default();
        d.power(true);
        d.write(false, &[0x11, 0x29, 0x2c])?;
        d.write(true, &[0xf8, 0])?;
        let before = d.rgb();
        let generation = d.generation;
        for _ in 0..200 {
            d.write(false, &[0x34, 0x35])?;
            d.write(true, &[0])?;
            d.write(false, &[0xfe])?;
            d.write(true, &[1; 32])?;
        }
        assert_eq!(d.rgb(), before);
        assert_eq!(d.generation, generation);
        assert_eq!(d.diagnostics.ignored_commands[&0x34], 200);
        assert_eq!(d.diagnostics.unknown_commands[&0xfe], 200);
        assert_eq!(d.diagnostics.ignored_data_bytes[&0xfe], 6400);
        assert_eq!(d.diagnostics.journal.len(), 128);
        assert!(d.parameters.is_empty());
        Ok(())
    }
    #[test]
    fn strict_is_opt_in_and_survives_panel_reset() -> Result<()> {
        let mut d = Display::default();
        d.power(true);
        d.set_strict(true);
        d.write(false, &[0x34, 0x35])?;
        d.write(true, &[0])?;
        d.write(false, &[0x01])?;
        assert!(d.write(false, &[0xfe]).is_err());
        assert_eq!(d.diagnostics.ignored_commands[&0x35], 1);
        assert_eq!(d.diagnostics.unknown_commands[&0xfe], 1);
        Ok(())
    }
}
