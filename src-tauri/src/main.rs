// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    if std::env::args().any(|arg| arg == "--codex-quota-probe") {
        match sysmon_tray_lib::probe_codex_quota() {
            Ok(result) => println!("{result}"),
            Err(error) => { eprintln!("{error}"); std::process::exit(1); }
        }
        return;
    }
    sysmon_tray_lib::run()
}
