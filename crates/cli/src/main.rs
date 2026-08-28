use syscalls::{StdFilesystem, SystemGit};

fn main() {
    let args = std::env::args();
    match cli::run(args, &StdFilesystem, &SystemGit, &SystemGit) {
        Ok(output) => println!("{output}"),
        Err(err) => {
            eprintln!("error: {err}");
            std::process::exit(1);
        }
    }
}
