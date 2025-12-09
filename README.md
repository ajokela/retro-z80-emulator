# RetroShield Z80 Emulator (Rust)

A Z80 emulator written in Rust for testing [RetroShield Z80](https://8bitforce.com/about/) firmware. Includes a simple passthrough emulator, a full-featured TUI debugger, and a WebAssembly build for browser-based emulation.

## Features

- **Z80 emulation** using the [rz80](https://crates.io/crates/rz80) crate
- **Dual serial chip emulation:**
  - MC6850 ACIA (ports $80/$81) - used by MINT, Firth, Monty, Retro Pascal
  - Intel 8251 USART (ports $00/$01) - used by Grant's BASIC, EFEX
- **Three emulator modes:**
  - `retroshield` - Simple passthrough (stdin/stdout)
  - `retroshield_tui` - Full TUI debugger with registers, disassembly, stack, memory view
  - **WebAssembly** - Browser-based emulation with JavaScript API

## Building

### Native (TUI and Passthrough)

```bash
cargo build --release
```

Binaries will be in `target/release/`.

### WebAssembly

To build for the browser, you'll need [wasm-pack](https://rustwasm.github.io/wasm-pack/):

```bash
# Install wasm-pack if you don't have it
cargo install wasm-pack

# Build the WASM package
wasm-pack build --target web --out-dir pkg
```

This produces:
- `pkg/retro_z80_emulator.js` - JavaScript bindings
- `pkg/retro_z80_emulator_bg.wasm` - WebAssembly binary

#### Using in a Web Page

```html
<script type="module">
import init, { Z80Emulator } from './pkg/retro_z80_emulator.js';

async function main() {
    await init();
    const emulator = new Z80Emulator();

    // Load a ROM
    const response = await fetch('rom.bin');
    const data = new Uint8Array(await response.arrayBuffer());
    emulator.load_rom(data);

    // For 8251-based ROMs (like Grant's BASIC)
    // emulator.set_8251_mode(true);

    // Run emulation loop
    function runLoop() {
        emulator.run(50000);  // Run 50000 cycles

        // Get any output from the serial port
        const output = emulator.get_output_string();
        if (output.length > 0) {
            console.log(output);
        }

        requestAnimationFrame(runLoop);
    }
    runLoop();

    // Send keyboard input
    document.addEventListener('keydown', (e) => {
        if (e.key === 'Enter') {
            emulator.send_char(13);
        } else if (e.key.length === 1) {
            emulator.send_char(e.key.charCodeAt(0));
        }
    });
}

main();
</script>
```

#### WASM API

| Method | Description |
|--------|-------------|
| `new Z80Emulator()` | Create a new emulator instance |
| `load_rom(data: Uint8Array)` | Load ROM data and reset CPU |
| `reset()` | Reset the CPU |
| `run(cycles: number)` | Execute given number of cycles |
| `send_char(c: number)` | Send a character to serial input |
| `send_string(s: string)` | Send a string to serial input |
| `get_output_string()` | Get and clear serial output buffer |
| `set_8251_mode(enabled: boolean)` | Switch between ACIA and 8251 mode |
| `get_pc()` | Get program counter |
| `get_cycles()` | Get total cycles executed |
| `is_halted()` | Check if CPU is halted |

## Usage

### Passthrough Emulator

Simple emulator that connects stdin/stdout directly to the emulated serial port:

```bash
./target/release/retroshield [OPTIONS] <rom.bin>

Options:
  -d          Debug mode (prints load info)
  -c <cycles> Run for specified cycles then exit
```

### TUI Debugger

Full-screen debugger with register display, disassembly, stack view, memory view, and terminal:

```bash
./target/release/retroshield_tui <rom.bin>
```

## TUI Layout

```
┌─ Registers ─────────┬─ Disassembly ──────────┬─ Stack ─────────┐
│ PC:0075  SP:FFC2    │ >0075: LD A,($2043)    │>FFC2: 0075      │
│ AF:0042  BC:010D    │  0078: CP $00          │ FFC4: 0120      │
│ DE:215C  HL:20A6    │  007A: JR Z,$0075      │ FFC6: 0000      │
│ IX:0000  IY:0000    │                        ├─ CPU State ─────┤
│ Flags: -Z----N-     │                        │ IM:1 IFF1:1     │
├─ Memory @ $2000 ────┤                        │ HALT:0 I:00 R:7F│
│ 2000: 00 0D 50 52...├────────────────────────┴─────────────────┤
│ 2010: 57 20 57 4F...│ Terminal                                 │
│ ...                 │ Z80 BASIC Ver 4.7b                       │
│                     │ Ok                                       │
│                     │ █                                        │
└─────────────────────┴──────────────────────────────────────────┘
[RUNNING] Z80:31.16MHz CPU:5.8% Mem:8.5MB F5:Run F6:Step F12:Quit
```

## TUI Controls

| Key | Action |
|-----|--------|
| **F5** | Run continuously |
| **F6** | Step one instruction |
| **F7** | Pause execution |
| **F8** | Reset CPU |
| **F9/F10** | Memory view scroll up/down |
| **PgUp/PgDn** | Memory view scroll (16 lines) |
| **Alt+=/Alt+-** | Adjust emulation speed |
| **F12** | Quit |
| **Other keys** | Send to emulated terminal |

The TUI starts in **paused** mode. Press **F5** to run or **F6** to step.

## Status Bar

The status bar shows:
- **State** - RUNNING, PAUSED, or HALTED
- **Z80 MHz** - Effective emulated clock speed
- **CPU%** - Host CPU usage
- **Mem MB** - Host memory usage
- **Cyc** - Total Z80 cycles executed

## I/O Ports

### MC6850 ACIA (ports $80/$81)

| Port | Read | Write |
|------|------|-------|
| $80 | Status register | Control register |
| $81 | Receive data | Transmit data |

### Intel 8251 USART (ports $00/$01)

| Port | Read | Write |
|------|------|-------|
| $00 | Receive data | Transmit data |
| $01 | Status register | Mode/Command register |

## Interrupt Support

- **IM 1** - Manually simulated (RST 38H) for 8251-based ROMs
- **IM 2** - Supported via rz80 crate

## Included ROMs

The `roms/` directory contains pre-built ROM binaries for testing:

| ROM | Description | Serial | Source |
|-----|-------------|--------|--------|
| `mint.z80.bin` | MINT interpreter | ACIA | [kz80_mint](https://gitlab.com/ajokela/retroshield-arduino/-/tree/master/kz80/kz80_mint) |
| `firth.z80.bin` | Firth Forth | ACIA | [jhlagado/firth](https://github.com/jhlagado/firth) |
| `monty.z80.bin` | Monty interpreter | ACIA | [kz80_monty](https://gitlab.com/ajokela/retroshield-arduino/-/tree/master/kz80/kz80_monty) |
| `pascal.bin` | Retro Pascal | ACIA | [retro-pascal](https://github.com/ajokela/retro-pascal) |
| `grantz80_basic_new.bin` | Grant's BASIC 4.7b | 8251 | [kz80_grantz80](https://gitlab.com/ajokela/retroshield-arduino/-/tree/master/kz80/kz80_grantz80) |
| `basic_gs47b.bin` | Grant Searle BASIC | 8251 | [Grant Searle](http://searle.x10host.com/z80/SimpleZ80.html) |
| `efex.bin` | EFEX monitor | 8251 | [kz80_efex](https://gitlab.com/ajokela/retroshield-arduino/-/tree/master/kz80/kz80_efex) |

## Examples

```bash
# Run MINT
./target/release/retroshield roms/mint.z80.bin

# Run Grant's BASIC with TUI
./target/release/retroshield_tui roms/grantz80_basic_new.bin

# Run Firth Forth
./target/release/retroshield roms/firth.z80.bin

# Run Retro Pascal with TUI
./target/release/retroshield_tui roms/pascal.bin
```

## Dependencies

### Native
- [rz80](https://crates.io/crates/rz80) - Z80 CPU emulation
- [ratatui](https://crates.io/crates/ratatui) - Terminal UI framework
- [crossterm](https://crates.io/crates/crossterm) - Terminal manipulation
- [sysinfo](https://crates.io/crates/sysinfo) - System metrics
- [libc](https://crates.io/crates/libc) - Non-blocking stdin (Unix)

### WebAssembly
- [wasm-bindgen](https://crates.io/crates/wasm-bindgen) - JavaScript interop

## License

MIT License - see [LICENSE](LICENSE)

## See Also

- [RetroShield Z80](https://8bitforce.com/about/) - Hardware platform by 8bitforce
- [retroshield-arduino](https://gitlab.com/ajokela/retroshield-arduino) - Arduino sketches for RetroShield
- [C Emulator](../) - Original C implementation with notcurses TUI
