//! Report repository code size and OS separation without changing source.
mod code_limits;

fn main() {
    match code_limits::entry(std::env::args_os().skip(1).collect()) {
        Ok((report, code)) => {
            println!("{report}");
            std::process::exit(code);
        }
        Err(error) => {
            eprintln!("Code limits measurement failed: {error}");
            std::process::exit(2);
        }
    }
}
