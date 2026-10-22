// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("--scan-helper") {
        std::process::exit(fine_file_lib::run_scan_helper(&args[2..]));
    }
    if args.get(1).map(String::as_str) == Some("--admin-helper") {
        std::process::exit(fine_file_lib::run_admin_helper(&args[2..]));
    }
    if args.get(1).map(String::as_str) == Some("--mcp") {
        std::process::exit(fine_file_lib::run_mcp(&args[2..]));
    }
    fine_file_lib::run()
}
