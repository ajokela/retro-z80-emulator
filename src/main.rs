//! RetroShield Z80 Emulator
//!
//! A Z80 emulator for testing RetroShield firmware.
//! Supports MC6850 ACIA and Intel 8251 USART serial chips.

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

/// RetroShield system with memory and I/O
#[allow(dead_code)]
struct RetroShield {
    rom_size: u16,
    acia: Mc6850,
    usart: Intel8251,
    uses_8251: bool,
    debug: bool,
}

impl RetroShield {
    fn new() -> Self {
        Self {
            rom_size: 0x2000, // Default 8KB ROM
            acia: Mc6850::new(),
            usart: Intel8251::new(),
            uses_8251: false,
            debug: false,
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
        // For now, we handle output directly
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
    eprintln!("Usage: {} [-d] [-c cycles] <rom.bin>", program);
    eprintln!("  -d          Debug mode");
    eprintln!("  -c cycles   Max cycles to run (0 = unlimited)");
}

fn main() {
    let args: Vec<String> = env::args().collect();

    let mut debug = false;
    let mut max_cycles: u64 = 0;
    let mut rom_file: Option<String> = None;

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

    // Initialize CPU with 64KB RAM
    let mut cpu = CPU::new_64k();

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
