use std::fs;
use std::io;

fn is_file(entry: &io::Result<fs::DirEntry>) -> bool {
    let m = entry.and_then(|e| e.metadata());
    match m {
        Ok(metadata) => metadata.is_file(),
        Err(_) => false,
    }
}

fn main() {
    match fs::read_dir("./") {
        Ok(entries) => {
            for entry in entries.filter(is_file) {
                let e = entry.unwrap();
                println!("{:?}", e.path());
            }
        },
        Err(_) => println!("Error!"),
    }
}
