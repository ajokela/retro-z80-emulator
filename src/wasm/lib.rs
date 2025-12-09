//! WASM Z80 Emulator for RetroShield
//!
//! A browser-based Z80 emulator using wasm-bindgen.

#![cfg(target_arch = "wasm32")]

use std::cell::RefCell;
use std::collections::VecDeque;
use wasm_bindgen::prelude::*;
use rz80::{Bus, CPU};

/// MC6850 ACIA I/O ports
const ACIA_CTRL: u8 = 0x80;
const ACIA_DATA: u8 = 0x81;

/// Intel 8251 USART I/O ports
const USART_DATA: u8 = 0x00;
const USART_CTRL: u8 = 0x01;

/// MC6850 status register bits
const ACIA_RDRF: u8 = 0x01;  // Receive Data Register Full
const ACIA_TDRE: u8 = 0x02;  // Transmit Data Register Empty

/// Intel 8251 status register bits
const STAT_8251_TXRDY: u8 = 0x01;
const STAT_8251_RXRDY: u8 = 0x02;
const STAT_8251_TXE: u8   = 0x04;
const STAT_DSR: u8        = 0x80;
const USART_STATUS_INIT: u8 = STAT_8251_TXRDY | STAT_8251_TXE | STAT_DSR;

/// Terminal output callback
#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = console)]
    fn log(s: &str);
}

/// RetroShield system with memory and I/O
struct RetroShield {
    rx_buffer: RefCell<VecDeque<u8>>,
    tx_buffer: RefCell<Vec<u8>>,
    uses_8251: bool,
    int_signaled: RefCell<bool>,
}

impl RetroShield {
    fn new() -> Self {
        Self {
            rx_buffer: RefCell::new(VecDeque::new()),
            tx_buffer: RefCell::new(Vec::new()),
            uses_8251: false,
            int_signaled: RefCell::new(false),
        }
    }

    /// Check if we should trigger an interrupt (8251 mode with input available)
    fn should_interrupt(&self) -> bool {
        self.uses_8251 && !self.rx_buffer.borrow().is_empty() && !*self.int_signaled.borrow()
    }

    /// Mark interrupt as signaled
    fn set_int_signaled(&self, signaled: bool) {
        *self.int_signaled.borrow_mut() = signaled;
    }

    fn push_input(&self, c: u8) {
        self.rx_buffer.borrow_mut().push_back(c);
    }

    fn take_output(&self) -> Vec<u8> {
        std::mem::take(&mut *self.tx_buffer.borrow_mut())
    }

    fn read_acia_status(&self) -> u8 {
        let mut status = ACIA_TDRE;
        if !self.rx_buffer.borrow().is_empty() {
            status |= ACIA_RDRF;
        }
        status
    }

    fn read_acia_data(&self) -> u8 {
        self.rx_buffer.borrow_mut().pop_front().unwrap_or(0)
    }

    fn read_usart_status(&self) -> u8 {
        let mut status = USART_STATUS_INIT;
        if !self.rx_buffer.borrow().is_empty() {
            status |= STAT_8251_RXRDY;
        }
        status
    }

    fn read_usart_data(&self) -> u8 {
        let c = self.rx_buffer.borrow_mut().pop_front().unwrap_or(0);
        // Clear interrupt signal when data is read
        self.set_int_signaled(false);
        // Convert lowercase to uppercase like Arduino does
        if c >= b'a' && c <= b'z' {
            c - b'a' + b'A'
        } else {
            c
        }
    }

    fn write_data(&self, val: u8) {
        self.tx_buffer.borrow_mut().push(val);
    }
}

impl Bus for RetroShield {
    fn cpu_inp(&self, port: i32) -> i32 {
        let port = port as u8;
        let val = match port {
            ACIA_CTRL => self.read_acia_status(),
            ACIA_DATA => self.read_acia_data(),
            USART_CTRL => self.read_usart_status(),
            USART_DATA => self.read_usart_data(),
            _ => 0xFF,
        };
        val as i32
    }

    fn cpu_outp(&self, port: i32, val: i32) {
        let port = port as u8;
        let val = val as u8;
        match port {
            ACIA_CTRL | USART_CTRL => { /* Control register - ignored */ }
            ACIA_DATA | USART_DATA => self.write_data(val),
            _ => {}
        }
    }
}

/// WASM-exposed Z80 Emulator
#[wasm_bindgen]
pub struct Z80Emulator {
    cpu: CPU,
    system: RetroShield,
    total_cycles: u64,
    halted: bool,
}

#[wasm_bindgen]
impl Z80Emulator {
    /// Create a new emulator instance
    #[wasm_bindgen(constructor)]
    pub fn new() -> Z80Emulator {
        Z80Emulator {
            cpu: CPU::new_64k(),
            system: RetroShield::new(),
            total_cycles: 0,
            halted: false,
        }
    }

    /// Load ROM data into memory
    #[wasm_bindgen]
    pub fn load_rom(&mut self, data: &[u8]) {
        for (addr, &byte) in data.iter().enumerate() {
            if addr < 0x10000 {
                self.cpu.mem.w8(addr as i32, byte as i32);
            }
        }
        // Reset CPU state
        self.cpu.reset();
        self.total_cycles = 0;
        self.halted = false;
    }

    /// Reset the CPU
    #[wasm_bindgen]
    pub fn reset(&mut self) {
        self.cpu.reset();
        self.total_cycles = 0;
        self.halted = false;
        // Clear buffers and interrupt state
        self.system.rx_buffer.borrow_mut().clear();
        self.system.tx_buffer.borrow_mut().clear();
        self.system.set_int_signaled(false);
    }

    /// Run for a specified number of cycles
    /// Returns the actual number of cycles executed
    #[wasm_bindgen]
    pub fn run(&mut self, max_cycles: u32) -> u32 {
        if self.halted {
            return 0;
        }

        let mut cycles_run: u32 = 0;

        while cycles_run < max_cycles && !self.halted {
            // Check for 8251 interrupts - must trigger before each instruction
            if self.system.should_interrupt() && self.cpu.iff1 {
                let im = self.cpu.reg.im;
                if im == 1 {
                    // IM 1: RST 38H - disable interrupts, push PC, jump to $0038
                    self.cpu.iff1 = false;
                    self.cpu.iff2 = false;

                    // Push PC to stack
                    let pc = self.cpu.reg.pc();
                    let sp = self.cpu.reg.sp().wrapping_sub(2);
                    self.cpu.reg.set_sp(sp);
                    self.cpu.mem.w8(sp as i32, (pc & 0xFF) as i32);
                    self.cpu.mem.w8((sp.wrapping_add(1)) as i32, ((pc >> 8) & 0xFF) as i32);

                    // Jump to RST 38H vector
                    self.cpu.reg.set_pc(0x0038);

                    // Mark interrupt as signaled
                    self.system.set_int_signaled(true);
                } else if im == 2 {
                    // IM 2: Use rz80's built-in IRQ handling
                    self.cpu.irq();
                    self.system.set_int_signaled(true);
                }
            }

            let cycles = self.cpu.step(&self.system);
            cycles_run += cycles as u32;
            self.total_cycles += cycles as u64;

            if self.cpu.halt {
                self.halted = true;
                break;
            }
        }

        cycles_run
    }

    /// Send a character to the emulator
    #[wasm_bindgen]
    pub fn send_char(&mut self, c: u8) {
        self.system.push_input(c);
    }

    /// Send a string to the emulator
    #[wasm_bindgen]
    pub fn send_string(&mut self, s: &str) {
        for c in s.bytes() {
            self.system.push_input(c);
        }
    }

    /// Get output from the emulator (clears output buffer)
    #[wasm_bindgen]
    pub fn get_output(&mut self) -> Vec<u8> {
        self.system.take_output()
    }

    /// Get output as a string
    #[wasm_bindgen]
    pub fn get_output_string(&mut self) -> String {
        let bytes = self.system.take_output();
        String::from_utf8_lossy(&bytes).into_owned()
    }

    /// Check if CPU is halted
    #[wasm_bindgen]
    pub fn is_halted(&self) -> bool {
        self.halted
    }

    /// Get current PC
    #[wasm_bindgen]
    pub fn get_pc(&self) -> u16 {
        self.cpu.reg.pc() as u16
    }

    /// Get total cycles executed
    #[wasm_bindgen]
    pub fn get_cycles(&self) -> u64 {
        self.total_cycles
    }

    /// Read memory at address
    #[wasm_bindgen]
    pub fn read_memory(&self, addr: u16) -> u8 {
        self.cpu.mem.r8(addr as i32) as u8
    }

    /// Set whether to use Intel 8251 mode (for Grant's BASIC, etc.)
    #[wasm_bindgen]
    pub fn set_8251_mode(&mut self, enabled: bool) {
        self.system.uses_8251 = enabled;
    }
}

impl Default for Z80Emulator {
    fn default() -> Self {
        Self::new()
    }
}
