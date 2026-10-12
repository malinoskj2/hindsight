//! The executable; everything lives in the library.

#![cfg_attr(windows, windows_subsystem = "windows")]

fn main() -> anyhow::Result<()> {
    hindsight::run()
}
