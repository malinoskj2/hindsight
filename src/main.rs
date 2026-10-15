//! The executable. Everything else is in the library.

#![cfg_attr(windows, windows_subsystem = "windows")]

fn main() -> anyhow::Result<()> {
    hindsight::run()
}
