//! SD Card Emulation
//!
//! Emulates SD card storage via I/O ports 0x10-0x18.
//! Includes DMA block transfer support for CP/M disk operations.

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

/// DMA block transfer ports (for CP/M)
pub const SD_DMA_LO: u8 = 0x16;      // DMA address low byte
pub const SD_DMA_HI: u8 = 0x17;      // DMA address high byte
pub const SD_BLOCK_CMD: u8 = 0x18;   // Block command: 0=read 128 bytes, 1=write 128 bytes
pub const SD_SEEK_EX: u8 = 0x19;     // Seek position extended byte (bits 16-23)

/// Block size for DMA transfers
pub const BLOCK_SIZE: usize = 128;

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
    seek_pos: u32,     // 24-bit seek position (supports up to 16MB)
    // DMA block transfer state
    dma_addr: u16,
    block_status: u8,  // Status of last block operation
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
            dma_addr: 0x0080,  // Default CP/M DMA address
            block_status: 0,
        }
    }
}

/// SD Card emulator
pub struct SdCard {
    state: RefCell<SdState>,
    storage_dir: PathBuf,
    debug: bool,
    /// Reference to CPU memory for DMA block transfers (using rz80::Memory)
    cpu_mem: RefCell<Option<*mut rz80::Memory>>,
}

impl SdCard {
    pub fn new(storage_dir: PathBuf) -> Self {
        Self {
            state: RefCell::new(SdState::default()),
            storage_dir,
            debug: false,
            cpu_mem: RefCell::new(None),
        }
    }

    pub fn set_debug(&mut self, debug: bool) {
        self.debug = debug;
    }

    /// Set CPU memory reference for DMA block transfers
    /// Safety: The memory pointer must remain valid for the lifetime of the emulation
    pub fn set_cpu_mem(&self, mem: &mut rz80::Memory) {
        *self.cpu_mem.borrow_mut() = Some(mem as *mut rz80::Memory);
    }

    /// Perform DMA block read: read BLOCK_SIZE bytes from file to memory at dma_addr
    fn do_block_read(&self, state: &mut SdState) {
        let mem_ptr = *self.cpu_mem.borrow();
        if mem_ptr.is_none() {
            if self.debug {
                eprintln!("[SD] Block read failed: CPU memory not set");
            }
            state.block_status = 1;  // Error
            return;
        }

        if let Some(ref mut file) = state.file {
            let mut buffer = [0u8; BLOCK_SIZE];
            match file.read(&mut buffer) {
                Ok(bytes_read) => {
                    // Fill remaining with zeros if less than BLOCK_SIZE
                    for i in bytes_read..BLOCK_SIZE {
                        buffer[i] = 0;
                    }

                    // Copy to CPU memory at DMA address
                    let dma = state.dma_addr as usize;

                    // Safety: We trust the caller set up valid memory
                    unsafe {
                        let mem = &mut *mem_ptr.unwrap();
                        for i in 0..BLOCK_SIZE {
                            if dma + i < 0x10000 {
                                mem.w8((dma + i) as i32, buffer[i] as i32);
                            }
                        }
                    }

                    state.block_status = 0;  // Success
                    if self.debug {
                        eprintln!("[SD] Block read: {} bytes to DMA {:04X}", bytes_read, dma);
                    }
                }
                Err(e) => {
                    state.block_status = 1;  // Error
                    if self.debug {
                        eprintln!("[SD] Block read error: {}", e);
                    }
                }
            }
        } else {
            state.block_status = 1;  // Error - no file open
            if self.debug {
                eprintln!("[SD] Block read failed: no file open");
            }
        }
    }

    /// Perform DMA block write: write BLOCK_SIZE bytes from memory at dma_addr to file
    fn do_block_write(&self, state: &mut SdState) {
        let mem_ptr = *self.cpu_mem.borrow();
        if mem_ptr.is_none() {
            if self.debug {
                eprintln!("[SD] Block write failed: CPU memory not set");
            }
            state.block_status = 1;  // Error
            return;
        }

        if let Some(ref mut file) = state.file {
            let mut buffer = [0u8; BLOCK_SIZE];
            let dma = state.dma_addr as usize;

            // Copy from CPU memory at DMA address
            // Safety: We trust the caller set up valid memory
            unsafe {
                let mem = &*mem_ptr.unwrap();
                for i in 0..BLOCK_SIZE {
                    if dma + i < 0x10000 {
                        buffer[i] = mem.r8((dma + i) as i32) as u8;
                    }
                }
            }

            match file.write_all(&buffer) {
                Ok(_) => {
                    state.block_status = 0;  // Success
                    if self.debug {
                        eprintln!("[SD] Block write: {} bytes from DMA {:04X}", BLOCK_SIZE, dma);
                    }
                }
                Err(e) => {
                    state.block_status = 1;  // Error
                    if self.debug {
                        eprintln!("[SD] Block write error: {}", e);
                    }
                }
            }
        } else {
            state.block_status = 1;  // Error - no file open
            if self.debug {
                eprintln!("[SD] Block write failed: no file open");
            }
        }
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
            // DMA block transfer status (0 = success, non-zero = error)
            SD_BLOCK_CMD => state.block_status,
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
                state.seek_pos = (state.seek_pos & 0xFFFF00) | (val as u32);
                if self.debug {
                    eprintln!("[SD] Seek position low: {:02X} (pos={})", val, state.seek_pos);
                }
            }
            SD_SEEK_HI => {
                state.seek_pos = (state.seek_pos & 0xFF00FF) | ((val as u32) << 8);
                if self.debug {
                    eprintln!("[SD] Seek position high: {:02X} (pos={})", val, state.seek_pos);
                }
            }
            SD_SEEK_EX => {
                state.seek_pos = (state.seek_pos & 0x00FFFF) | ((val as u32) << 16);
                if self.debug {
                    eprintln!("[SD] Seek position ext: {:02X} (pos={})", val, state.seek_pos);
                }
            }
            // DMA address ports
            SD_DMA_LO => {
                state.dma_addr = (state.dma_addr & 0xFF00) | (val as u16);
                if self.debug {
                    eprintln!("[SD] DMA address low: {:02X} (addr={:04X})", val, state.dma_addr);
                }
            }
            SD_DMA_HI => {
                state.dma_addr = (state.dma_addr & 0x00FF) | ((val as u16) << 8);
                if self.debug {
                    eprintln!("[SD] DMA address high: {:02X} (addr={:04X})", val, state.dma_addr);
                }
            }
            // DMA block command: 0 = read 128 bytes, 1 = write 128 bytes
            SD_BLOCK_CMD => {
                if val == 0 {
                    self.do_block_read(&mut state);
                } else {
                    self.do_block_write(&mut state);
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
                    let pos = state.seek_pos as u64;
                    let _ = file.seek(SeekFrom::Start(pos));
                    state.status = STATUS_READY;
                    if self.debug {
                        eprintln!("[SD] Seeked to position {} (0x{:06X})", pos, pos);
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
        matches!(port, SD_CMD_PORT | SD_STATUS_PORT | SD_DATA_PORT | SD_FNAME_PORT |
                       SD_SEEK_LO | SD_SEEK_HI | SD_SEEK_EX | SD_DMA_LO | SD_DMA_HI | SD_BLOCK_CMD)
    }
}
