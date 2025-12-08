//! RetroShield Z80 Emulator - TUI Debugger
//!
//! Full-screen debugger with registers, disassembly, memory view, and terminal.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::env;
use std::fs::File;
use std::io::{self, Read};
use std::process;
use std::time::{Duration, Instant};

use crossterm::{
    event::{self, Event, KeyCode, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
    Frame, Terminal,
};
use rz80::{Bus, CPU};
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};

mod serial;
use serial::{Intel8251, Mc6850};

//=============================================================================
// Constants
//=============================================================================

/// MC6850 ACIA I/O ports
const ACIA_CTRL: u8 = 0x80;
const ACIA_DATA: u8 = 0x81;

/// Intel 8251 USART I/O ports
const USART_DATA: u8 = 0x00;
const USART_CTRL: u8 = 0x01;

/// Terminal buffer size
const TERM_COLS: usize = 80;
const TERM_ROWS: usize = 24;

//=============================================================================
// Terminal Emulation
//=============================================================================

struct TerminalBuffer {
    buffer: Vec<char>,
    cursor_x: usize,
    cursor_y: usize,
}

impl TerminalBuffer {
    fn new() -> Self {
        Self {
            buffer: vec![' '; TERM_COLS * TERM_ROWS],
            cursor_x: 0,
            cursor_y: 0,
        }
    }

    fn clear(&mut self) {
        self.buffer.fill(' ');
        self.cursor_x = 0;
        self.cursor_y = 0;
    }

    fn scroll(&mut self) {
        // Move lines up
        for y in 0..TERM_ROWS - 1 {
            for x in 0..TERM_COLS {
                self.buffer[y * TERM_COLS + x] = self.buffer[(y + 1) * TERM_COLS + x];
            }
        }
        // Clear last line
        for x in 0..TERM_COLS {
            self.buffer[(TERM_ROWS - 1) * TERM_COLS + x] = ' ';
        }
    }

    fn putchar(&mut self, c: char) {
        match c {
            '\r' => {
                self.cursor_x = 0;
            }
            '\n' => {
                self.cursor_y += 1;
                if self.cursor_y >= TERM_ROWS {
                    self.scroll();
                    self.cursor_y = TERM_ROWS - 1;
                }
            }
            '\x08' => {
                // Backspace
                if self.cursor_x > 0 {
                    self.cursor_x -= 1;
                }
            }
            '\x0C' => {
                // Form feed - clear screen
                self.clear();
            }
            '\x1B' => {
                // Escape - ignore for now
            }
            _ if c >= ' ' => {
                if self.cursor_x < TERM_COLS && self.cursor_y < TERM_ROWS {
                    self.buffer[self.cursor_y * TERM_COLS + self.cursor_x] = c;
                    self.cursor_x += 1;
                    if self.cursor_x >= TERM_COLS {
                        self.cursor_x = 0;
                        self.cursor_y += 1;
                        if self.cursor_y >= TERM_ROWS {
                            self.scroll();
                            self.cursor_y = TERM_ROWS - 1;
                        }
                    }
                }
            }
            _ => {}
        }
    }

    fn get_cursor(&self) -> (usize, usize) {
        (self.cursor_x, self.cursor_y)
    }

    fn get_lines(&self, max_lines: usize) -> Vec<String> {
        let mut lines = Vec::new();
        let start = if TERM_ROWS > max_lines {
            TERM_ROWS - max_lines
        } else {
            0
        };
        for y in start..TERM_ROWS {
            let line: String = (0..TERM_COLS)
                .map(|x| self.buffer[y * TERM_COLS + x])
                .collect();
            lines.push(line.trim_end().to_string());
        }
        lines
    }
}

//=============================================================================
// RetroShield System
//=============================================================================

struct RetroShield {
    rom_size: u16,
    acia: Mc6850,
    usart: Intel8251,
    terminal: RefCell<TerminalBuffer>,
    input_buffer: RefCell<VecDeque<u8>>,
    output_buffer: RefCell<VecDeque<u8>>,  // Buffered output for throttled display
    uses_8251: RefCell<bool>,
    int_signaled: RefCell<bool>,
}

impl RetroShield {
    fn new() -> Self {
        Self {
            rom_size: 0x2000,
            acia: Mc6850::new(),
            usart: Intel8251::new(),
            terminal: RefCell::new(TerminalBuffer::new()),
            input_buffer: RefCell::new(VecDeque::new()),
            output_buffer: RefCell::new(VecDeque::new()),
            uses_8251: RefCell::new(false),
            int_signaled: RefCell::new(false),
        }
    }

    /// Queue a character for output (called by cpu_outp)
    fn queue_output(&self, c: u8) {
        self.output_buffer.borrow_mut().push_back(c);
    }

    /// Consume up to `max_chars` from output buffer into terminal
    fn flush_output(&self, max_chars: usize) -> usize {
        let mut output = self.output_buffer.borrow_mut();
        let mut terminal = self.terminal.borrow_mut();
        let count = max_chars.min(output.len());
        for _ in 0..count {
            if let Some(c) = output.pop_front() {
                terminal.putchar(c as char);
            }
        }
        count
    }

    /// Get number of pending output characters
    fn pending_output(&self) -> usize {
        self.output_buffer.borrow().len()
    }

    fn configure_rom(&mut self, filename: &str) {
        let basename = filename.rsplit('/').next().unwrap_or(filename);
        if basename.contains("mint") {
            self.rom_size = 0x0800; // 2KB ROM for MINT
        } else {
            self.rom_size = 0x2000; // Default 8KB
        }
    }

    fn send_key(&self, c: u8) {
        self.input_buffer.borrow_mut().push_back(c);
        *self.int_signaled.borrow_mut() = false; // Allow new interrupt
    }

    fn input_available(&self) -> bool {
        !self.input_buffer.borrow().is_empty()
    }

    fn get_input(&self) -> Option<u8> {
        let result = self.input_buffer.borrow_mut().pop_front();
        if result.is_some() {
            *self.int_signaled.borrow_mut() = false; // Allow new interrupt
        }
        result
    }

    fn uses_8251(&self) -> bool {
        *self.uses_8251.borrow()
    }

    fn should_interrupt(&self) -> bool {
        self.uses_8251() && self.input_available() && !*self.int_signaled.borrow()
    }

    fn mark_interrupt_sent(&self) {
        *self.int_signaled.borrow_mut() = true;
    }

    fn get_terminal_lines(&self, max_lines: usize) -> Vec<String> {
        self.terminal.borrow().get_lines(max_lines)
    }

    fn get_cursor(&self) -> (usize, usize) {
        self.terminal.borrow().get_cursor()
    }
}

impl Bus for RetroShield {
    fn cpu_inp(&self, port: i32) -> i32 {
        let port = port as u8;
        let val = match port {
            ACIA_CTRL => {
                let mut status = 0x02; // TDRE always set
                if self.input_available() {
                    status |= 0x01; // RDRF
                }
                status
            }
            ACIA_DATA => self.get_input().unwrap_or(0),
            USART_CTRL => {
                *self.uses_8251.borrow_mut() = true; // Mark ROM as using 8251
                let mut status = 0x85; // TxRDY + TxE + DSR
                if self.input_available() {
                    status |= 0x02; // RxRDY
                }
                status
            }
            USART_DATA => {
                *self.uses_8251.borrow_mut() = true; // Mark ROM as using 8251
                let c = self.get_input().unwrap_or(0);
                // Convert to uppercase like Arduino
                if c >= b'a' && c <= b'z' {
                    c - b'a' + b'A'
                } else {
                    c
                }
            }
            _ => 0xFF,
        };
        val as i32
    }

    fn cpu_outp(&self, port: i32, val: i32) {
        let port = port as u8;
        let val = val as u8;
        match port {
            ACIA_DATA => {
                self.queue_output(val);
            }
            USART_DATA => {
                *self.uses_8251.borrow_mut() = true;
                self.queue_output(val);
            }
            USART_CTRL => {
                *self.uses_8251.borrow_mut() = true;
                // Mode/command register - ignored
            }
            _ => {}
        }
    }
}

//=============================================================================
// Disassembler (simplified)
//=============================================================================

fn disassemble_instruction(cpu: &CPU, addr: u16) -> (String, u8) {
    let opcode = cpu.mem.r8(addr as i32) as u8;
    let byte1 = cpu.mem.r8((addr.wrapping_add(1)) as i32) as u8;
    let byte2 = cpu.mem.r8((addr.wrapping_add(2)) as i32) as u8;
    let word = (byte2 as u16) << 8 | byte1 as u16;

    // Simplified disassembler - just common opcodes
    let (mnemonic, len) = match opcode {
        0x00 => ("NOP".to_string(), 1),
        0x01 => (format!("LD BC,${:04X}", word), 3),
        0x02 => ("LD (BC),A".to_string(), 1),
        0x03 => ("INC BC".to_string(), 1),
        0x04 => ("INC B".to_string(), 1),
        0x05 => ("DEC B".to_string(), 1),
        0x06 => (format!("LD B,${:02X}", byte1), 2),
        0x07 => ("RLCA".to_string(), 1),
        0x08 => ("EX AF,AF'".to_string(), 1),
        0x09 => ("ADD HL,BC".to_string(), 1),
        0x0A => ("LD A,(BC)".to_string(), 1),
        0x0B => ("DEC BC".to_string(), 1),
        0x0C => ("INC C".to_string(), 1),
        0x0D => ("DEC C".to_string(), 1),
        0x0E => (format!("LD C,${:02X}", byte1), 2),
        0x0F => ("RRCA".to_string(), 1),
        0x10 => (format!("DJNZ ${:04X}", addr.wrapping_add(2).wrapping_add(byte1 as i8 as u16)), 2),
        0x11 => (format!("LD DE,${:04X}", word), 3),
        0x12 => ("LD (DE),A".to_string(), 1),
        0x13 => ("INC DE".to_string(), 1),
        0x14 => ("INC D".to_string(), 1),
        0x15 => ("DEC D".to_string(), 1),
        0x16 => (format!("LD D,${:02X}", byte1), 2),
        0x17 => ("RLA".to_string(), 1),
        0x18 => (format!("JR ${:04X}", addr.wrapping_add(2).wrapping_add(byte1 as i8 as u16)), 2),
        0x19 => ("ADD HL,DE".to_string(), 1),
        0x1A => ("LD A,(DE)".to_string(), 1),
        0x1B => ("DEC DE".to_string(), 1),
        0x1C => ("INC E".to_string(), 1),
        0x1D => ("DEC E".to_string(), 1),
        0x1E => (format!("LD E,${:02X}", byte1), 2),
        0x1F => ("RRA".to_string(), 1),
        0x20 => (format!("JR NZ,${:04X}", addr.wrapping_add(2).wrapping_add(byte1 as i8 as u16)), 2),
        0x21 => (format!("LD HL,${:04X}", word), 3),
        0x22 => (format!("LD (${:04X}),HL", word), 3),
        0x23 => ("INC HL".to_string(), 1),
        0x24 => ("INC H".to_string(), 1),
        0x25 => ("DEC H".to_string(), 1),
        0x26 => (format!("LD H,${:02X}", byte1), 2),
        0x27 => ("DAA".to_string(), 1),
        0x28 => (format!("JR Z,${:04X}", addr.wrapping_add(2).wrapping_add(byte1 as i8 as u16)), 2),
        0x29 => ("ADD HL,HL".to_string(), 1),
        0x2A => (format!("LD HL,(${:04X})", word), 3),
        0x2B => ("DEC HL".to_string(), 1),
        0x2C => ("INC L".to_string(), 1),
        0x2D => ("DEC L".to_string(), 1),
        0x2E => (format!("LD L,${:02X}", byte1), 2),
        0x2F => ("CPL".to_string(), 1),
        0x30 => (format!("JR NC,${:04X}", addr.wrapping_add(2).wrapping_add(byte1 as i8 as u16)), 2),
        0x31 => (format!("LD SP,${:04X}", word), 3),
        0x32 => (format!("LD (${:04X}),A", word), 3),
        0x33 => ("INC SP".to_string(), 1),
        0x34 => ("INC (HL)".to_string(), 1),
        0x35 => ("DEC (HL)".to_string(), 1),
        0x36 => (format!("LD (HL),${:02X}", byte1), 2),
        0x37 => ("SCF".to_string(), 1),
        0x38 => (format!("JR C,${:04X}", addr.wrapping_add(2).wrapping_add(byte1 as i8 as u16)), 2),
        0x39 => ("ADD HL,SP".to_string(), 1),
        0x3A => (format!("LD A,(${:04X})", word), 3),
        0x3B => ("DEC SP".to_string(), 1),
        0x3C => ("INC A".to_string(), 1),
        0x3D => ("DEC A".to_string(), 1),
        0x3E => (format!("LD A,${:02X}", byte1), 2),
        0x3F => ("CCF".to_string(), 1),
        // LD r,r' instructions (0x40-0x7F except 0x76)
        0x40..=0x75 | 0x77..=0x7F => {
            let regs = ["B", "C", "D", "E", "H", "L", "(HL)", "A"];
            let dst = ((opcode - 0x40) >> 3) as usize;
            let src = ((opcode - 0x40) & 7) as usize;
            (format!("LD {},{}", regs[dst], regs[src]), 1)
        }
        0x76 => ("HALT".to_string(), 1),
        // ALU ops (0x80-0xBF)
        0x80..=0xBF => {
            let ops = ["ADD A,", "ADC A,", "SUB ", "SBC A,", "AND ", "XOR ", "OR ", "CP "];
            let regs = ["B", "C", "D", "E", "H", "L", "(HL)", "A"];
            let op = ((opcode - 0x80) >> 3) as usize;
            let reg = ((opcode - 0x80) & 7) as usize;
            (format!("{}{}", ops[op], regs[reg]), 1)
        }
        0xC0 => ("RET NZ".to_string(), 1),
        0xC1 => ("POP BC".to_string(), 1),
        0xC2 => (format!("JP NZ,${:04X}", word), 3),
        0xC3 => (format!("JP ${:04X}", word), 3),
        0xC4 => (format!("CALL NZ,${:04X}", word), 3),
        0xC5 => ("PUSH BC".to_string(), 1),
        0xC6 => (format!("ADD A,${:02X}", byte1), 2),
        0xC7 => ("RST $00".to_string(), 1),
        0xC8 => ("RET Z".to_string(), 1),
        0xC9 => ("RET".to_string(), 1),
        0xCA => (format!("JP Z,${:04X}", word), 3),
        0xCB => (format!("CB ${:02X}", byte1), 2), // CB prefix
        0xCC => (format!("CALL Z,${:04X}", word), 3),
        0xCD => (format!("CALL ${:04X}", word), 3),
        0xCE => (format!("ADC A,${:02X}", byte1), 2),
        0xCF => ("RST $08".to_string(), 1),
        0xD0 => ("RET NC".to_string(), 1),
        0xD1 => ("POP DE".to_string(), 1),
        0xD2 => (format!("JP NC,${:04X}", word), 3),
        0xD3 => (format!("OUT (${:02X}),A", byte1), 2),
        0xD4 => (format!("CALL NC,${:04X}", word), 3),
        0xD5 => ("PUSH DE".to_string(), 1),
        0xD6 => (format!("SUB ${:02X}", byte1), 2),
        0xD7 => ("RST $10".to_string(), 1),
        0xD8 => ("RET C".to_string(), 1),
        0xD9 => ("EXX".to_string(), 1),
        0xDA => (format!("JP C,${:04X}", word), 3),
        0xDB => (format!("IN A,(${:02X})", byte1), 2),
        0xDC => (format!("CALL C,${:04X}", word), 3),
        0xDD => (format!("DD ${:02X}", byte1), 2), // DD prefix (IX)
        0xDE => (format!("SBC A,${:02X}", byte1), 2),
        0xDF => ("RST $18".to_string(), 1),
        0xE0 => ("RET PO".to_string(), 1),
        0xE1 => ("POP HL".to_string(), 1),
        0xE2 => (format!("JP PO,${:04X}", word), 3),
        0xE3 => ("EX (SP),HL".to_string(), 1),
        0xE4 => (format!("CALL PO,${:04X}", word), 3),
        0xE5 => ("PUSH HL".to_string(), 1),
        0xE6 => (format!("AND ${:02X}", byte1), 2),
        0xE7 => ("RST $20".to_string(), 1),
        0xE8 => ("RET PE".to_string(), 1),
        0xE9 => ("JP (HL)".to_string(), 1),
        0xEA => (format!("JP PE,${:04X}", word), 3),
        0xEB => ("EX DE,HL".to_string(), 1),
        0xEC => (format!("CALL PE,${:04X}", word), 3),
        0xED => (format!("ED ${:02X}", byte1), 2), // ED prefix
        0xEE => (format!("XOR ${:02X}", byte1), 2),
        0xEF => ("RST $28".to_string(), 1),
        0xF0 => ("RET P".to_string(), 1),
        0xF1 => ("POP AF".to_string(), 1),
        0xF2 => (format!("JP P,${:04X}", word), 3),
        0xF3 => ("DI".to_string(), 1),
        0xF4 => (format!("CALL P,${:04X}", word), 3),
        0xF5 => ("PUSH AF".to_string(), 1),
        0xF6 => (format!("OR ${:02X}", byte1), 2),
        0xF7 => ("RST $30".to_string(), 1),
        0xF8 => ("RET M".to_string(), 1),
        0xF9 => ("LD SP,HL".to_string(), 1),
        0xFA => (format!("JP M,${:04X}", word), 3),
        0xFB => ("EI".to_string(), 1),
        0xFC => (format!("CALL M,${:04X}", word), 3),
        0xFD => (format!("FD ${:02X}", byte1), 2), // FD prefix (IY)
        0xFE => (format!("CP ${:02X}", byte1), 2),
        0xFF => ("RST $38".to_string(), 1),
    };

    (mnemonic, len)
}

//=============================================================================
// Application State
//=============================================================================

struct App {
    cpu: CPU,
    system: RetroShield,
    paused: bool,
    total_cycles: u64,
    cycles_per_frame: u32,
    chars_per_frame: usize,  // Output throttle: max chars to display per frame
    mem_view_addr: u16,
    last_update: Instant,
    cycles_since_update: u64,
    effective_mhz: f64,
    // Host metrics
    sysinfo: System,
    pid: Pid,
    host_cpu_percent: f32,
    host_memory_mb: f64,
    // Cursor blink
    cursor_visible: bool,
    last_blink: Instant,
}

impl App {
    fn new(rom_file: &str) -> io::Result<Self> {
        let mut system = RetroShield::new();
        system.configure_rom(rom_file);

        let mut cpu = CPU::new_64k();

        // Load ROM
        let mut file = File::open(rom_file)?;
        let mut buffer = Vec::new();
        file.read_to_end(&mut buffer)?;

        for (addr, &byte) in buffer.iter().enumerate() {
            if addr < 0x10000 {
                cpu.mem.w8(addr as i32, byte as i32);
            }
        }

        let pid = Pid::from_u32(std::process::id());
        let mut sysinfo = System::new();
        sysinfo.refresh_processes_specifics(
            ProcessesToUpdate::Some(&[pid]),
            true,
            ProcessRefreshKind::new().with_memory().with_cpu(),
        );

        Ok(Self {
            cpu,
            system,
            paused: true,
            total_cycles: 0,
            cycles_per_frame: 50000,
            chars_per_frame: 120,  // ~120 chars/frame * 60fps = ~7200 chars/sec (readable speed)
            mem_view_addr: 0x2000,
            last_update: Instant::now(),
            cycles_since_update: 0,
            effective_mhz: 0.0,
            sysinfo,
            pid,
            host_cpu_percent: 0.0,
            host_memory_mb: 0.0,
            cursor_visible: true,
            last_blink: Instant::now(),
        })
    }

    fn update_cursor_blink(&mut self) {
        if self.last_blink.elapsed() >= Duration::from_millis(500) {
            self.cursor_visible = !self.cursor_visible;
            self.last_blink = Instant::now();
        }
    }

    /// Flush buffered output to terminal at throttled rate
    fn flush_output(&mut self) {
        self.system.flush_output(self.chars_per_frame);
    }

    fn step(&mut self) {
        let cycles = self.cpu.step(&self.system);
        self.total_cycles += cycles as u64;
        self.cycles_since_update += cycles as u64;

        // Trigger interrupt for 8251 ROMs when input is available
        // Check after step so any EI instruction has taken effect
        if self.system.should_interrupt() && self.cpu.iff1 {
            // rz80 only supports IM 2, so we manually handle IM 0/1
            let im = self.cpu.reg.im;
            if im == 2 {
                self.cpu.irq();
            } else if im == 1 {
                // IM 1: RST 38H - push PC and jump to $0038
                self.cpu.iff1 = false;
                self.cpu.iff2 = false;
                let pc = self.cpu.reg.pc();
                let sp = self.cpu.reg.sp().wrapping_sub(2);
                self.cpu.reg.set_sp(sp);
                self.cpu.mem.w8(sp, pc & 0xFF);
                self.cpu.mem.w8(sp + 1, (pc >> 8) & 0xFF);
                self.cpu.reg.set_pc(0x0038);
            }
            // IM 0 not commonly used, skip for now
            self.system.mark_interrupt_sent();
        }
    }

    fn run_frame(&mut self) {
        for _ in 0..self.cycles_per_frame {
            if self.cpu.halt {
                break;
            }
            self.step();
        }
    }

    fn update_metrics(&mut self) {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_update);
        if elapsed.as_millis() >= 500 {
            self.effective_mhz =
                self.cycles_since_update as f64 / elapsed.as_secs_f64() / 1_000_000.0;
            self.cycles_since_update = 0;
            self.last_update = now;

            // Update host metrics
            self.sysinfo.refresh_processes_specifics(
                ProcessesToUpdate::Some(&[self.pid]),
                true,
                ProcessRefreshKind::new().with_memory().with_cpu(),
            );
            if let Some(process) = self.sysinfo.process(self.pid) {
                self.host_cpu_percent = process.cpu_usage();
                self.host_memory_mb = process.memory() as f64 / (1024.0 * 1024.0);
            }
        }
    }

    fn reset(&mut self) {
        self.cpu.reg.reset();
        self.total_cycles = 0;
        self.cycles_since_update = 0;
        self.system.terminal.borrow_mut().clear();
    }
}

//=============================================================================
// UI Rendering
//=============================================================================

fn render_registers(f: &mut Frame, area: Rect, cpu: &CPU) {
    let pc = cpu.reg.pc() as u16;
    let sp = cpu.reg.sp() as u16;
    let af = cpu.reg.af() as u16;
    let bc = cpu.reg.bc() as u16;
    let de = cpu.reg.de() as u16;
    let hl = cpu.reg.hl() as u16;
    let ix = cpu.reg.ix() as u16;
    let iy = cpu.reg.iy() as u16;
    let flags = cpu.reg.f() as u8;

    let flag_s = if flags & 0x80 != 0 { "S" } else { "-" };
    let flag_z = if flags & 0x40 != 0 { "Z" } else { "-" };
    let flag_h = if flags & 0x10 != 0 { "H" } else { "-" };
    let flag_pv = if flags & 0x04 != 0 { "P" } else { "-" };
    let flag_n = if flags & 0x02 != 0 { "N" } else { "-" };
    let flag_c = if flags & 0x01 != 0 { "C" } else { "-" };

    let text = vec![
        Line::from(vec![
            Span::styled("PC:", Style::default().fg(Color::Gray)),
            Span::styled(format!("{:04X}", pc), Style::default().fg(Color::Green)),
            Span::raw("  "),
            Span::styled("SP:", Style::default().fg(Color::Gray)),
            Span::styled(format!("{:04X}", sp), Style::default().fg(Color::White)),
        ]),
        Line::from(vec![
            Span::styled("AF:", Style::default().fg(Color::Gray)),
            Span::styled(format!("{:04X}", af), Style::default().fg(Color::White)),
            Span::raw("  "),
            Span::styled("BC:", Style::default().fg(Color::Gray)),
            Span::styled(format!("{:04X}", bc), Style::default().fg(Color::White)),
        ]),
        Line::from(vec![
            Span::styled("DE:", Style::default().fg(Color::Gray)),
            Span::styled(format!("{:04X}", de), Style::default().fg(Color::White)),
            Span::raw("  "),
            Span::styled("HL:", Style::default().fg(Color::Gray)),
            Span::styled(format!("{:04X}", hl), Style::default().fg(Color::White)),
        ]),
        Line::from(vec![
            Span::styled("IX:", Style::default().fg(Color::Gray)),
            Span::styled(format!("{:04X}", ix), Style::default().fg(Color::White)),
            Span::raw("  "),
            Span::styled("IY:", Style::default().fg(Color::Gray)),
            Span::styled(format!("{:04X}", iy), Style::default().fg(Color::White)),
        ]),
        Line::from(vec![
            Span::styled("Flags: ", Style::default().fg(Color::Gray)),
            Span::styled(
                format!("{}{}-{}-{}{}{}", flag_s, flag_z, flag_h, flag_pv, flag_n, flag_c),
                Style::default().fg(Color::Yellow),
            ),
        ]),
    ];

    let block = Block::default()
        .title(" Registers ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));

    let paragraph = Paragraph::new(text).block(block);
    f.render_widget(paragraph, area);
}

fn render_disassembly(f: &mut Frame, area: Rect, cpu: &CPU) {
    let pc = cpu.reg.pc() as u16;
    let mut addr = pc.saturating_sub(6);
    let mut lines = Vec::new();
    let visible_lines = (area.height as usize).saturating_sub(2);

    for _ in 0..visible_lines {
        let (mnemonic, len) = disassemble_instruction(cpu, addr);

        // Build hex bytes string
        let mut hex = String::new();
        for i in 0..len {
            hex.push_str(&format!("{:02X} ", cpu.mem.r8((addr.wrapping_add(i as u16)) as i32)));
        }

        let is_current = addr == pc;
        let marker = if is_current { ">" } else { " " };

        let line = Line::from(vec![
            Span::styled(
                marker,
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!("{:04X}: ", addr), Style::default().fg(Color::DarkGray)),
            Span::styled(format!("{:<12}", hex), Style::default().fg(Color::Gray)),
            Span::styled(
                mnemonic,
                if is_current {
                    Style::default()
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::White)
                },
            ),
        ]);
        lines.push(line);

        addr = addr.wrapping_add(len as u16);
    }

    let block = Block::default()
        .title(" Disassembly ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));

    let paragraph = Paragraph::new(lines).block(block);
    f.render_widget(paragraph, area);
}

fn render_memory(f: &mut Frame, area: Rect, cpu: &CPU, start_addr: u16) {
    let mut lines = Vec::new();
    let visible_lines = (area.height as usize).saturating_sub(2);
    let mut addr = start_addr;

    for _ in 0..visible_lines {
        let mut hex = String::new();
        let mut ascii = String::new();

        for i in 0..16 {
            let byte = cpu.mem.r8((addr.wrapping_add(i)) as i32) as u8;
            hex.push_str(&format!("{:02X} ", byte));
            ascii.push(if byte >= 0x20 && byte < 0x7F {
                byte as char
            } else {
                '.'
            });
        }

        let line = Line::from(vec![
            Span::styled(format!("{:04X}: ", addr), Style::default().fg(Color::DarkGray)),
            Span::styled(hex, Style::default().fg(Color::Rgb(136, 170, 204))),
            Span::styled(ascii, Style::default().fg(Color::Rgb(170, 204, 170))),
        ]);
        lines.push(line);

        addr = addr.wrapping_add(16);
    }

    let block = Block::default()
        .title(format!(" Memory @ ${:04X} ", start_addr))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));

    let paragraph = Paragraph::new(lines).block(block);
    f.render_widget(paragraph, area);
}

fn render_stack(f: &mut Frame, area: Rect, cpu: &CPU) {
    let mut lines = Vec::new();
    let visible_lines = (area.height as usize).saturating_sub(2);
    let sp = cpu.reg.sp() as u16;

    for i in 0..visible_lines {
        let addr = sp.wrapping_add((i * 2) as u16);
        let lo = cpu.mem.r8(addr as i32) as u8;
        let hi = cpu.mem.r8(addr.wrapping_add(1) as i32) as u8;
        let word = ((hi as u16) << 8) | (lo as u16);

        let marker = if i == 0 { ">" } else { " " };

        let line = Line::from(vec![
            Span::styled(marker, Style::default().fg(Color::Green)),
            Span::styled(format!("{:04X}: ", addr), Style::default().fg(Color::DarkGray)),
            Span::styled(format!("{:04X}", word), Style::default().fg(Color::White)),
        ]);
        lines.push(line);
    }

    let block = Block::default()
        .title(format!(" Stack @ ${:04X} ", sp))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));

    let paragraph = Paragraph::new(lines).block(block);
    f.render_widget(paragraph, area);
}

fn render_cpu_state(f: &mut Frame, area: Rect, cpu: &CPU) {
    let im = cpu.reg.im;
    let iff1 = if cpu.iff1 { "1" } else { "0" };
    let iff2 = if cpu.iff2 { "1" } else { "0" };
    let halt = if cpu.halt { "1" } else { "0" };
    let r = cpu.reg.r as u8;
    let i = cpu.reg.i as u8;

    let lines = vec![
        Line::from(vec![
            Span::styled("IM:", Style::default().fg(Color::Gray)),
            Span::styled(format!("{} ", im), Style::default().fg(Color::Yellow)),
            Span::styled("IFF1:", Style::default().fg(Color::Gray)),
            Span::styled(format!("{} ", iff1), Style::default().fg(Color::White)),
            Span::styled("IFF2:", Style::default().fg(Color::Gray)),
            Span::styled(format!("{}", iff2), Style::default().fg(Color::White)),
        ]),
        Line::from(vec![
            Span::styled("HALT:", Style::default().fg(Color::Gray)),
            Span::styled(format!("{} ", halt), Style::default().fg(if cpu.halt { Color::Red } else { Color::White })),
            Span::styled("I:", Style::default().fg(Color::Gray)),
            Span::styled(format!("{:02X} ", i), Style::default().fg(Color::White)),
            Span::styled("R:", Style::default().fg(Color::Gray)),
            Span::styled(format!("{:02X}", r), Style::default().fg(Color::White)),
        ]),
    ];

    let block = Block::default()
        .title(" CPU State ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));

    let paragraph = Paragraph::new(lines).block(block);
    f.render_widget(paragraph, area);
}

fn render_terminal(f: &mut Frame, area: Rect, system: &RetroShield, cursor_visible: bool) {
    let visible_lines = (area.height as usize).saturating_sub(2);
    let term_lines = system.get_terminal_lines(visible_lines);
    let (cursor_x, cursor_y) = system.get_cursor();

    // Calculate which line the cursor is on relative to visible area
    let start_line = if TERM_ROWS > visible_lines {
        TERM_ROWS - visible_lines
    } else {
        0
    };
    let cursor_line_in_view = if cursor_y >= start_line {
        Some(cursor_y - start_line)
    } else {
        None
    };

    let lines: Vec<Line> = term_lines
        .iter()
        .enumerate()
        .map(|(line_idx, s)| {
            // Check if cursor is on this line and visible
            if cursor_visible && cursor_line_in_view == Some(line_idx) {
                // Build line with cursor
                let mut spans = Vec::new();
                let chars: Vec<char> = s.chars().collect();

                if cursor_x > 0 {
                    let before: String = chars.iter().take(cursor_x).collect();
                    spans.push(Span::styled(before, Style::default().fg(Color::White)));
                }

                // Cursor character (block cursor)
                let cursor_char = if cursor_x < chars.len() {
                    chars[cursor_x]
                } else {
                    ' '
                };
                spans.push(Span::styled(
                    cursor_char.to_string(),
                    Style::default().fg(Color::Black).bg(Color::Green),
                ));

                // After cursor
                if cursor_x + 1 < chars.len() {
                    let after: String = chars.iter().skip(cursor_x + 1).collect();
                    spans.push(Span::styled(after, Style::default().fg(Color::White)));
                }

                Line::from(spans)
            } else {
                Line::from(Span::styled(s.clone(), Style::default().fg(Color::White)))
            }
        })
        .collect();

    let block = Block::default()
        .title(" Terminal ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));

    let paragraph = Paragraph::new(lines).block(block);
    f.render_widget(paragraph, area);
}

fn render_status(f: &mut Frame, area: Rect, app: &App) {
    let status_text = if app.cpu.halt {
        Span::styled("[HALTED]", Style::default().fg(Color::Red).add_modifier(Modifier::BOLD))
    } else if app.paused {
        Span::styled("[PAUSED]", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD))
    } else {
        Span::styled("[RUNNING]", Style::default().fg(Color::Green).add_modifier(Modifier::BOLD))
    };

    let help = " F5:Run F6:Step F7:Pause F8:Reset F9/10:Mem Alt+/-:Speed F12:Quit";

    // Show pending output buffer size if significant
    let pending = app.system.pending_output();
    let pending_text = if pending > 100 {
        Span::styled(format!("Buf:{} ", pending), Style::default().fg(Color::Yellow))
    } else {
        Span::raw("")
    };

    let line = Line::from(vec![
        status_text,
        Span::raw(" "),
        Span::styled(
            format!("Z80:{:.2}MHz ", app.effective_mhz),
            Style::default().fg(Color::Cyan),
        ),
        Span::styled(
            format!("CPU:{:.1}% ", app.host_cpu_percent),
            Style::default().fg(Color::LightMagenta),
        ),
        Span::styled(
            format!("Mem:{:.1}MB ", app.host_memory_mb),
            Style::default().fg(Color::LightBlue),
        ),
        pending_text,
        Span::styled(
            format!("Cyc:{}", app.total_cycles),
            Style::default().fg(Color::Gray),
        ),
        Span::styled(help, Style::default().fg(Color::DarkGray)),
    ]);

    let paragraph = Paragraph::new(line);
    f.render_widget(paragraph, area);
}

fn ui(f: &mut Frame, app: &App) {
    let size = f.area();

    // Main layout: top area for panels, bottom for status
    let main_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(10), Constraint::Length(1)])
        .split(size);

    // Top area: left (registers+memory) and right (disasm+stack+state+terminal)
    let top_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(40), Constraint::Percentage(60)])
        .split(main_chunks[0]);

    // Left side: registers on top, memory below
    let left_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(8), Constraint::Min(6)])
        .split(top_chunks[0]);

    // Right side: upper area (disasm+stack+state) and terminal below
    let right_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(45), Constraint::Percentage(55)])
        .split(top_chunks[1]);

    // Upper right: disassembly on left, stack+state on right
    let upper_right_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(60), Constraint::Percentage(40)])
        .split(right_chunks[0]);

    // Stack and CPU state stacked vertically
    let stack_state_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(6), Constraint::Length(4)])
        .split(upper_right_chunks[1]);

    render_registers(f, left_chunks[0], &app.cpu);
    render_memory(f, left_chunks[1], &app.cpu, app.mem_view_addr);
    render_disassembly(f, upper_right_chunks[0], &app.cpu);
    render_stack(f, stack_state_chunks[0], &app.cpu);
    render_cpu_state(f, stack_state_chunks[1], &app.cpu);
    render_terminal(f, right_chunks[1], &app.system, app.cursor_visible);
    render_status(f, main_chunks[1], app);
}

//=============================================================================
// Main
//=============================================================================

fn print_usage(program: &str) {
    eprintln!("Usage: {} <rom.bin>", program);
    eprintln!();
    eprintln!("TUI Debugger Controls:");
    eprintln!("  F5        Run continuously");
    eprintln!("  F6        Step one instruction");
    eprintln!("  F7        Pause execution");
    eprintln!("  F8        Reset CPU");
    eprintln!("  F9/F10    Memory view scroll up/down");
    eprintln!("  PgUp/PgDn Memory view scroll (16 lines)");
    eprintln!("  +/-       Adjust run speed");
    eprintln!("  F12       Quit");
    eprintln!("  Other     Send to emulated terminal");
}

fn main() -> io::Result<()> {
    let args: Vec<String> = env::args().collect();

    if args.len() < 2 {
        print_usage(&args[0]);
        process::exit(1);
    }

    let rom_file = &args[1];

    // Initialize app
    let mut app = App::new(rom_file)?;

    // Setup terminal
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    // Main loop
    let tick_rate = Duration::from_millis(16); // ~60 FPS
    let mut last_tick = Instant::now();

    loop {
        // Draw UI
        terminal.draw(|f| ui(f, &app))?;

        // Handle input with timeout
        let timeout = tick_rate.saturating_sub(last_tick.elapsed());
        if event::poll(timeout)? {
            if let Event::Key(key) = event::read()? {
                match key.code {
                    KeyCode::F(12) => break,
                    KeyCode::F(5) => app.paused = false,
                    KeyCode::F(6) => {
                        app.paused = true;
                        app.step();
                    }
                    KeyCode::F(7) => app.paused = true,
                    KeyCode::F(8) => app.reset(),
                    KeyCode::F(9) => {
                        app.mem_view_addr = app.mem_view_addr.saturating_sub(16);
                    }
                    KeyCode::F(10) => {
                        app.mem_view_addr = app.mem_view_addr.saturating_add(16);
                    }
                    KeyCode::PageUp => {
                        app.mem_view_addr = app.mem_view_addr.saturating_sub(256);
                    }
                    KeyCode::PageDown => {
                        app.mem_view_addr = app.mem_view_addr.saturating_add(256);
                    }
                    KeyCode::Char(c) => {
                        if key.modifiers.contains(KeyModifiers::CONTROL) {
                            // Ctrl+C sends 0x03, Ctrl+other sends control codes
                            let code = (c as u8) & 0x1F;
                            app.system.send_key(code);
                        } else if key.modifiers.contains(KeyModifiers::ALT) {
                            // Alt+= to increase speed, Alt+- to decrease
                            if c == '=' || c == '+' {
                                app.cycles_per_frame = (app.cycles_per_frame * 2).min(1_000_000);
                            } else if c == '-' {
                                app.cycles_per_frame = (app.cycles_per_frame / 2).max(1000);
                            }
                        } else {
                            // Send character to emulated system
                            app.system.send_key(c as u8);
                        }
                    }
                    KeyCode::Enter => app.system.send_key(b'\r'),
                    KeyCode::Backspace => app.system.send_key(0x08),
                    KeyCode::Esc => app.system.send_key(0x1B),
                    _ => {}
                }
            }
        }

        // Run emulation if not paused
        if last_tick.elapsed() >= tick_rate {
            if !app.paused && !app.cpu.halt {
                app.run_frame();
            }
            // Flush buffered output at throttled rate (always, even when paused)
            app.flush_output();
            app.update_metrics();
            app.update_cursor_blink();
            last_tick = Instant::now();
        }
    }

    // Restore terminal
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;

    Ok(())
}
