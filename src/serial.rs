//! Serial chip emulation for RetroShield
//!
//! Implements MC6850 ACIA and Intel 8251 USART

use std::cell::RefCell;
use std::collections::VecDeque;
use std::io::{self, Read};

/// Check if input is available on stdin (non-blocking)
fn stdin_has_data() -> bool {
    // On Unix, we can use select() or poll()
    // For simplicity, we'll check if stdin is a tty and has data
    #[cfg(unix)]
    {
        use std::os::unix::io::AsRawFd;

        let fd = io::stdin().as_raw_fd();
        unsafe {
            let mut fds: libc::fd_set = std::mem::zeroed();
            libc::FD_ZERO(&mut fds);
            libc::FD_SET(fd, &mut fds);

            let mut tv = libc::timeval {
                tv_sec: 0,
                tv_usec: 0,
            };

            libc::select(fd + 1, &mut fds, std::ptr::null_mut(), std::ptr::null_mut(), &mut tv) > 0
        }
    }

    #[cfg(not(unix))]
    {
        false // TODO: Windows implementation
    }
}

/// Read a character from stdin (non-blocking)
fn read_char() -> Option<u8> {
    if stdin_has_data() {
        let mut buf = [0u8; 1];
        if io::stdin().read(&mut buf).is_ok() {
            if buf[0] != 0 {
                return Some(buf[0]);
            }
        }
    }
    None
}

//=============================================================================
// MC6850 ACIA (Asynchronous Communications Interface Adapter)
//=============================================================================

/// MC6850 status register bits
const ACIA_RDRF: u8 = 0x01;  // Receive Data Register Full
const ACIA_TDRE: u8 = 0x02;  // Transmit Data Register Empty

/// MC6850 ACIA emulation
#[allow(dead_code)]
pub struct Mc6850 {
    control: RefCell<u8>,
    rx_buffer: RefCell<VecDeque<u8>>,
}

impl Mc6850 {
    pub fn new() -> Self {
        Self {
            control: RefCell::new(0),
            rx_buffer: RefCell::new(VecDeque::new()),
        }
    }

    /// Read status register (port $80)
    pub fn read_status(&self) -> u8 {
        let mut status = ACIA_TDRE; // Always ready to transmit

        // Check for input
        if let Some(c) = read_char() {
            self.rx_buffer.borrow_mut().push_back(c);
        }

        if !self.rx_buffer.borrow().is_empty() {
            status |= ACIA_RDRF;
        }

        status
    }

    /// Read data register (port $81)
    pub fn read_data(&self) -> u8 {
        // Check for new input first
        if let Some(c) = read_char() {
            self.rx_buffer.borrow_mut().push_back(c);
        }

        self.rx_buffer.borrow_mut().pop_front().unwrap_or(0)
    }

    /// Write control register (port $80)
    #[allow(dead_code)]
    pub fn write_control(&self, val: u8) {
        *self.control.borrow_mut() = val;
    }
}

//=============================================================================
// Intel 8251 USART (Universal Synchronous/Asynchronous Receiver/Transmitter)
//=============================================================================

/// Intel 8251 status register bits
const STAT_8251_TXRDY: u8 = 0x01;  // Transmitter Ready
const STAT_8251_RXRDY: u8 = 0x02;  // Receiver Ready
const STAT_8251_TXE: u8   = 0x04;  // Transmitter Empty
const STAT_DSR: u8        = 0x80;  // Data Set Ready

/// Initial status: TxRDY + TxE + DSR
const USART_STATUS_INIT: u8 = STAT_8251_TXRDY | STAT_8251_TXE | STAT_DSR;

/// Intel 8251 USART emulation
#[allow(dead_code)]
pub struct Intel8251 {
    mode: RefCell<u8>,
    command: RefCell<u8>,
    rx_buffer: RefCell<VecDeque<u8>>,
}

impl Intel8251 {
    pub fn new() -> Self {
        Self {
            mode: RefCell::new(0),
            command: RefCell::new(0),
            rx_buffer: RefCell::new(VecDeque::new()),
        }
    }

    /// Read status register (port $01)
    pub fn read_status(&self) -> u8 {
        let mut status = USART_STATUS_INIT;

        // Check for input
        if let Some(c) = read_char() {
            self.rx_buffer.borrow_mut().push_back(c);
        }

        if !self.rx_buffer.borrow().is_empty() {
            status |= STAT_8251_RXRDY;
        }

        status
    }

    /// Read data register (port $00)
    pub fn read_data(&self) -> u8 {
        // Check for new input first
        if let Some(c) = read_char() {
            self.rx_buffer.borrow_mut().push_back(c);
        }

        let c = self.rx_buffer.borrow_mut().pop_front().unwrap_or(0);

        // Convert lowercase to uppercase like Arduino does
        if c >= b'a' && c <= b'z' {
            c - b'a' + b'A'
        } else {
            c
        }
    }

    /// Write control/mode register (port $01)
    #[allow(dead_code)]
    pub fn write_control(&self, val: u8) {
        *self.command.borrow_mut() = val;
    }
}
