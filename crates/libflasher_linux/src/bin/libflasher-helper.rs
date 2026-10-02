//! `libflasher-helper`: the small program libflasher starts through pkexec
//! to open a disk for writing. See `libflasher_linux::helper`.

fn main() -> std::process::ExitCode {
    #[cfg(target_os = "linux")]
    return libflasher_linux::helper::main(std::env::args().skip(1));
    #[cfg(not(target_os = "linux"))]
    {
        eprintln!("libflasher-helper is only used on Linux");
        std::process::ExitCode::FAILURE
    }
}
