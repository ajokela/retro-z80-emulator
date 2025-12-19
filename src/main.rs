//! RetroShield Z80 Emulator
//!
//! A Z80 emulator for testing RetroShield firmware.
//! Supports MC6850 ACIA and Intel 8251 USART serial chips.

use std::cell::RefCell;
use std::env;
use std::fs::File;
use std::io::{self, Read, Write};
use std::process;

use rz80::{Bus, CPU};

mod serial;

use serial::{Mc6850, Intel8251};

/// MC6850 ACIA I/O ports
const ACIA_CTRL: u8 = 0x80;
const ACIA_DATA: u8 = 0x81;

/// Intel 8251 USART I/O ports
const USART_DATA: u8 = 0x00;
const USART_CTRL: u8 = 0x01;

/// Memory dump I/O ports
const DUMP_ADDR_LO: u8 = 0x82;   // Low byte of start address
const DUMP_ADDR_HI: u8 = 0x83;   // High byte of start address
const DUMP_LEN_LO: u8 = 0x84;    // Low byte of length
const DUMP_LEN_HI: u8 = 0x85;    // High byte of length
const DUMP_TRIGGER: u8 = 0x86;  // Write any value to trigger dump

/// Memory dump state (for interior mutability)
#[derive(Default)]
struct DumpState {
    start_addr: u16,
    length: u16,
    output_file: Option<String>,
}

/// RetroShield system with memory and I/O
#[allow(dead_code)]
struct RetroShield {
    rom_size: u16,
    acia: Mc6850,
    usart: Intel8251,
    uses_8251: bool,
    debug: bool,
    dump_state: RefCell<DumpState>,
    cpu_mem: RefCell<Option<*const rz80::Memory>>,  // Reference to CPU memory for dumps
}

impl RetroShield {
    fn new() -> Self {
        Self {
            rom_size: 0x2000, // Default 8KB ROM
            acia: Mc6850::new(),
            usart: Intel8251::new(),
            uses_8251: false,
            debug: false,
            dump_state: RefCell::new(DumpState::default()),
            cpu_mem: RefCell::new(None),
        }
    }

    fn set_dump_output(&self, filename: &str) {
        self.dump_state.borrow_mut().output_file = Some(filename.to_string());
    }

    fn set_cpu_mem(&self, mem: &rz80::Memory) {
        *self.cpu_mem.borrow_mut() = Some(mem as *const _);
    }

    fn do_memory_dump(&self) {
        let state = self.dump_state.borrow();
        let filename = state.output_file.as_ref().map(|s| s.as_str()).unwrap_or("dump.bin");
        let start = state.start_addr as usize;
        let len = state.length as usize;

        if len == 0 {
            eprintln!("Memory dump: length is 0, nothing to dump");
            return;
        }

        // Get CPU memory reference
        let mem_ptr = *self.cpu_mem.borrow();
        if mem_ptr.is_none() {
            eprintln!("Memory dump: CPU memory not available");
            return;
        }

        // Safety: We know the CPU memory is valid for the lifetime of the emulation
        let mem = unsafe { &*mem_ptr.unwrap() };

        // Read memory range
        let mut buffer = Vec::with_capacity(len);
        for addr in start..(start + len).min(0x10000) {
            buffer.push(mem.r8(addr as i32) as u8);
        }

        // Write to file
        match File::create(filename) {
            Ok(mut file) => {
                match file.write_all(&buffer) {
                    Ok(_) => eprintln!("Memory dump: {} bytes written to {} (0x{:04X}-0x{:04X})",
                                      buffer.len(), filename, start, start + buffer.len() - 1),
                    Err(e) => eprintln!("Memory dump: write error: {}", e),
                }
            }
            Err(e) => eprintln!("Memory dump: failed to create {}: {}", filename, e),
        }
    }

    /// Configure ROM size based on ROM filename
    fn configure_rom(&mut self, filename: &str) {
        let basename = filename.rsplit('/').next().unwrap_or(filename);

        if basename.contains("mint") {
            self.rom_size = 0x0800; // 2KB ROM for MINT
            if self.debug {
                eprintln!("MINT ROM: {} bytes protected", self.rom_size);
            }
        } else {
            self.rom_size = 0x2000; // Default 8KB
            if self.debug {
                eprintln!("Default ROM: {} bytes protected", self.rom_size);
            }
        }
    }
}

impl Bus for RetroShield {
    fn cpu_inp(&self, port: i32) -> i32 {
        let port = port as u8;
        let val = match port {
            // MC6850 ACIA
            ACIA_CTRL => self.acia.read_status(),
            ACIA_DATA => self.acia.read_data(),

            // Intel 8251 USART
            USART_CTRL => self.usart.read_status(),
            USART_DATA => self.usart.read_data(),

            _ => 0xFF,
        };
        val as i32
    }

    fn cpu_outp(&self, port: i32, val: i32) {
        let port = port as u8;
        let val = val as u8;
        // Note: We need interior mutability here since Bus trait takes &self
        // Using RefCell for dump state
        match port {
            // MC6850 ACIA
            ACIA_CTRL => { /* Control register write - ignored for now */ }
            ACIA_DATA => {
                print!("{}", val as char);
                let _ = io::stdout().flush();
            }

            // Intel 8251 USART
            USART_CTRL => { /* Control/mode register - ignored for now */ }
            USART_DATA => {
                print!("{}", val as char);
                let _ = io::stdout().flush();
            }

            // Memory dump ports
            DUMP_ADDR_LO => {
                let mut state = self.dump_state.borrow_mut();
                state.start_addr = (state.start_addr & 0xFF00) | (val as u16);
            }
            DUMP_ADDR_HI => {
                let mut state = self.dump_state.borrow_mut();
                state.start_addr = (state.start_addr & 0x00FF) | ((val as u16) << 8);
            }
            DUMP_LEN_LO => {
                let mut state = self.dump_state.borrow_mut();
                state.length = (state.length & 0xFF00) | (val as u16);
            }
            DUMP_LEN_HI => {
                let mut state = self.dump_state.borrow_mut();
                state.length = (state.length & 0x00FF) | ((val as u16) << 8);
            }
            DUMP_TRIGGER => {
                self.do_memory_dump();
            }

            _ => {}
        }
    }
}

fn load_rom(cpu: &mut CPU, filename: &str) -> io::Result<usize> {
    let mut file = File::open(filename)?;
    let mut buffer = Vec::new();
    let bytes_read = file.read_to_end(&mut buffer)?;

    // Load into CPU memory
    for (addr, &byte) in buffer.iter().enumerate() {
        if addr < 0x10000 {
            cpu.mem.w8(addr as i32, byte as i32);
        }
    }

    Ok(bytes_read)
}

fn print_usage(program: &str) {
    eprintln!("Usage: {} [-d] [-c cycles] [-o dump.bin] <rom.bin>", program);
    eprintln!("  -d          Debug mode");
    eprintln!("  -c cycles   Max cycles to run (0 = unlimited)");
    eprintln!("  -o file     Output file for memory dumps (default: dump.bin)");
}

fn main() {
    let args: Vec<String> = env::args().collect();

    let mut debug = false;
    let mut max_cycles: u64 = 0;
    let mut rom_file: Option<String> = None;
    let mut dump_output: Option<String> = None;

    // Parse arguments
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "-d" | "--debug" => debug = true,
            "-c" => {
                i += 1;
                if i < args.len() {
                    max_cycles = args[i].parse().unwrap_or(0);
                }
            }
            "-o" => {
                i += 1;
                if i < args.len() {
                    dump_output = Some(args[i].clone());
                }
            }
            arg if !arg.starts_with('-') => {
                rom_file = Some(arg.to_string());
            }
            _ => {}
        }
        i += 1;
    }

    let rom_file = match rom_file {
        Some(f) => f,
        None => {
            print_usage(&args[0]);
            process::exit(1);
        }
    };

    // Initialize system
    let mut system = RetroShield::new();
    system.debug = debug;
    system.configure_rom(&rom_file);

    // Set dump output file if specified
    if let Some(ref output) = dump_output {
        system.set_dump_output(output);
    }

    // Initialize CPU with 64KB RAM
    let mut cpu = CPU::new_64k();

    // Set CPU memory reference for dumps
    system.set_cpu_mem(&cpu.mem);

    // Load ROM
    match load_rom(&mut cpu, &rom_file) {
        Ok(bytes) => {
            if debug {
                eprintln!("Loaded {} bytes from {}", bytes, rom_file);
            }
        }
        Err(e) => {
            eprintln!("Failed to load ROM: {}", e);
            process::exit(1);
        }
    }

    if debug {
        eprintln!("Starting Z80 emulation...");
    }

    // Main emulation loop
    let mut total_cycles: u64 = 0;

    loop {
        let cycles = cpu.step(&system);
        total_cycles += cycles as u64;

        // Check for halt
        if cpu.halt {
            if debug {
                eprintln!("\nCPU halted at PC={:04X} after {} cycles",
                         cpu.reg.pc(), total_cycles);
            }
            break;
        }

        // Check cycle limit
        if max_cycles > 0 && total_cycles >= max_cycles {
            if debug {
                eprintln!("Stopped at PC={:04X} after {} cycles",
                         cpu.reg.pc(), total_cycles);
            }
            break;
        }
    }
}
