//! SD Card Emulation
//!
//! Emulates SD card storage via I/O ports 0x10-0x15.

use std::cell::RefCell;
use std::fs::{self, File, OpenOptions, ReadDir};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;

/// SD Card I/O ports
pub const SD_CMD_PORT: u8 = 0x10;
pub const SD_STATUS_PORT: u8 = 0x11;
pub const SD_DATA_PORT: u8 = 0x12;
pub const SD_FNAME_PORT: u8 = 0x13;
pub const SD_SEEK_LO: u8 = 0x14;
pub const SD_SEEK_HI: u8 = 0x15;

/// SD Commands
const CMD_OPEN_READ: u8 = 0x01;
const CMD_CREATE: u8 = 0x02;
const CMD_OPEN_APPEND: u8 = 0x03;
const CMD_SEEK_START: u8 = 0x04;
const CMD_CLOSE: u8 = 0x05;
const CMD_DIR: u8 = 0x06;
const CMD_OPEN_RW: u8 = 0x07;
const CMD_SEEK_BYTE: u8 = 0x08;
const CMD_SEEK_16: u8 = 0x09;

/// SD Status bits
const STATUS_READY: u8 = 0x01;
const STATUS_ERROR: u8 = 0x02;
const STATUS_DATA: u8 = 0x80;

/// Internal state for SD emulation
struct SdState {
    filename: String,
    filename_pos: usize,
    file: Option<File>,
    status: u8,
    dir: Option<ReadDir>,
    dir_entry: String,
    dir_entry_pos: usize,
    seek_pos: u16,
}

impl Default for SdState {
    fn default() -> Self {
        Self {
            filename: String::new(),
            filename_pos: 0,
            file: None,
            status: STATUS_READY,
            dir: None,
            dir_entry: String::new(),
            dir_entry_pos: 0,
            seek_pos: 0,
        }
    }
}

/// SD Card emulator
pub struct SdCard {
    state: RefCell<SdState>,
    storage_dir: PathBuf,
    debug: bool,
}

impl SdCard {
    pub fn new(storage_dir: PathBuf) -> Self {
        Self {
            state: RefCell::new(SdState::default()),
            storage_dir,
            debug: false,
        }
    }

    pub fn set_debug(&mut self, debug: bool) {
        self.debug = debug;
    }

    fn full_path(&self, filename: &str) -> PathBuf {
        self.storage_dir.join(filename)
    }

    /// Handle port read
    pub fn read_port(&self, port: u8) -> u8 {
        let mut state = self.state.borrow_mut();

        match port {
            SD_STATUS_PORT => {
                let mut status = state.status;
                if state.file.is_some() || state.dir.is_some() {
                    status |= STATUS_DATA;
                }
                status
            }
            SD_DATA_PORT => {
                // Read from file
                if let Some(ref mut file) = state.file {
                    let mut buf = [0u8; 1];
                    match file.read_exact(&mut buf) {
                        Ok(_) => buf[0],
                        Err(_) => {
                            state.file = None;
                            state.status = STATUS_READY;
                            0
                        }
                    }
                }
                // Read from directory listing
                else if state.dir.is_some() {
                    // Need next character from dir entry
                    if state.dir_entry_pos >= state.dir_entry.len() {
                        // Get next directory entry
                        loop {
                            if let Some(ref mut dir) = state.dir {
                                match dir.next() {
                                    Some(Ok(entry)) => {
                                        let name = entry.file_name();
                                        let name = name.to_string_lossy();
                                        if name == "." || name == ".." {
                                            continue;
                                        }
                                        state.dir_entry = format!("{}\r\n", name);
                                        state.dir_entry_pos = 0;
                                        break;
                                    }
                                    _ => {
                                        // End of directory
                                        state.dir = None;
                                        state.status = STATUS_READY;
                                        return 0;
                                    }
                                }
                            } else {
                                return 0;
                            }
                        }
                    }
                    // Return next character
                    let c = state.dir_entry.as_bytes()[state.dir_entry_pos];
                    state.dir_entry_pos += 1;
                    c
                } else {
                    0
                }
            }
            _ => 0xFF,
        }
    }

    /// Handle port write
    pub fn write_port(&self, port: u8, val: u8) {
        let mut state = self.state.borrow_mut();

        match port {
            SD_CMD_PORT => {
                self.handle_command(&mut state, val);
            }
            SD_DATA_PORT => {
                if let Some(ref mut file) = state.file {
                    let _ = file.write_all(&[val]);
                }
            }
            SD_FNAME_PORT => {
                if val == 0 {
                    // Null terminator - filename complete
                    state.filename_pos = 0;
                    if self.debug {
                        eprintln!("[SD] Filename set: {}", state.filename);
                    }
                } else {
                    state.filename.push(val as char);
                    state.filename_pos += 1;
                }
            }
            SD_SEEK_LO => {
                state.seek_pos = (state.seek_pos & 0xFF00) | (val as u16);
                if self.debug {
                    eprintln!("[SD] Seek position low: {} (pos={})", val, state.seek_pos);
                }
            }
            SD_SEEK_HI => {
                state.seek_pos = (state.seek_pos & 0x00FF) | ((val as u16) << 8);
                if self.debug {
                    eprintln!("[SD] Seek position high: {} (pos={})", val, state.seek_pos);
                }
            }
            _ => {}
        }
    }

    fn handle_command(&self, state: &mut SdState, cmd: u8) {
        match cmd {
            CMD_OPEN_READ => {
                let path = self.full_path(&state.filename);
                state.file = None;

                match File::open(&path) {
                    Ok(file) => {
                        state.file = Some(file);
                        state.status = STATUS_READY;
                        if self.debug {
                            eprintln!("[SD] Opened for read: {:?}", path);
                        }
                    }
                    Err(e) => {
                        state.status = STATUS_ERROR | STATUS_READY;
                        if self.debug {
                            eprintln!("[SD] Failed to open: {:?} ({})", path, e);
                        }
                    }
                }
                state.filename.clear();
            }
            CMD_CREATE => {
                let path = self.full_path(&state.filename);
                state.file = None;

                // Create storage directory if needed
                let _ = fs::create_dir_all(&self.storage_dir);

                match File::create(&path) {
                    Ok(file) => {
                        state.file = Some(file);
                        state.status = STATUS_READY;
                        if self.debug {
                            eprintln!("[SD] Created: {:?}", path);
                        }
                    }
                    Err(e) => {
                        state.status = STATUS_ERROR | STATUS_READY;
                        if self.debug {
                            eprintln!("[SD] Failed to create: {:?} ({})", path, e);
                        }
                    }
                }
                state.filename.clear();
            }
            CMD_OPEN_APPEND => {
                let path = self.full_path(&state.filename);
                state.file = None;

                match OpenOptions::new().read(true).write(true).open(&path) {
                    Ok(mut file) => {
                        let _ = file.seek(SeekFrom::End(0));
                        state.file = Some(file);
                        state.status = STATUS_READY;
                        if self.debug {
                            eprintln!("[SD] Opened for append: {:?}", path);
                        }
                    }
                    Err(e) => {
                        state.status = STATUS_ERROR | STATUS_READY;
                        if self.debug {
                            eprintln!("[SD] Failed to open for append: {:?} ({})", path, e);
                        }
                    }
                }
                state.filename.clear();
            }
            CMD_SEEK_START => {
                if let Some(ref mut file) = state.file {
                    let _ = file.seek(SeekFrom::Start(0));
                    state.status = STATUS_READY;
                    if self.debug {
                        eprintln!("[SD] Seeked to start");
                    }
                } else {
                    state.status = STATUS_ERROR | STATUS_READY;
                }
            }
            CMD_CLOSE => {
                state.file = None;
                state.dir = None;
                state.status = STATUS_READY;
                if self.debug {
                    eprintln!("[SD] Closed file");
                }
            }
            CMD_DIR => {
                state.dir = None;
                let _ = fs::create_dir_all(&self.storage_dir);

                match fs::read_dir(&self.storage_dir) {
                    Ok(dir) => {
                        state.dir = Some(dir);
                        state.dir_entry.clear();
                        state.dir_entry_pos = 0;
                        state.status = STATUS_READY;
                        if self.debug {
                            eprintln!("[SD] DIR: {:?}", self.storage_dir);
                        }
                    }
                    Err(_) => {
                        state.status = STATUS_ERROR | STATUS_READY;
                    }
                }
            }
            CMD_OPEN_RW => {
                let path = self.full_path(&state.filename);
                state.file = None;

                match OpenOptions::new().read(true).write(true).open(&path) {
                    Ok(file) => {
                        state.file = Some(file);
                        state.status = STATUS_READY;
                        if self.debug {
                            eprintln!("[SD] Opened for read/write: {:?}", path);
                        }
                    }
                    Err(e) => {
                        state.status = STATUS_ERROR | STATUS_READY;
                        if self.debug {
                            eprintln!("[SD] Failed to open for read/write: {:?} ({})", path, e);
                        }
                    }
                }
                state.filename.clear();
            }
            CMD_SEEK_BYTE | CMD_SEEK_16 => {
                if let Some(ref mut file) = state.file {
                    let _ = file.seek(SeekFrom::Start(state.seek_pos as u64));
                    state.status = STATUS_READY;
                    if self.debug {
                        eprintln!("[SD] Seeked to position {}", state.seek_pos);
                    }
                } else {
                    state.status = STATUS_ERROR | STATUS_READY;
                }
            }
            _ => {}
        }
    }

    /// Check if this port is handled by SD emulation
    pub fn handles_port(port: u8) -> bool {
        matches!(port, SD_CMD_PORT | SD_STATUS_PORT | SD_DATA_PORT | SD_FNAME_PORT | SD_SEEK_LO | SD_SEEK_HI)
    }
}
