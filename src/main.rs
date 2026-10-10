//! The executable: everything lives in the library, which this only starts.

#![cfg_attr(windows, windows_subsystem = "windows")]

fn main() -> anyhow::Result<()> {
    hindsight::run()
}
