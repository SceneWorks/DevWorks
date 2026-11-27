#![windows_subsystem = "windows"]

use std::os::windows::process::CommandExt;
use std::process::Command;

const CREATE_NO_WINDOW: u32 = 0x0800_0000;

fn main() -> std::io::Result<()> {
    let desktop = std::env::current_exe()?.with_file_name("devworks-desktop.exe");
    // The desktop retains its console subsystem for direct diagnostic and updater calls.
    Command::new(desktop)
        .args(std::env::args_os().skip(1))
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()?;
    Ok(())
}
