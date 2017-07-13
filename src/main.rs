use std::fs;
use std::io;

fn is_file(entry: &io::Result<fs::DirEntry>) -> bool {
    match entry.as_ref() {
      Err(_) => false,
      Ok(ref e) => {
        if let Ok(metadata) = e.metadata() {
            metadata.is_file()
        } else {
            false
        }
      }
    }
}

fn main() {
    match fs::read_dir("./") {
        Ok(entries) => {
            for entry in entries.filter(is_file) {
                let e = entry.unwrap();
                println!("{:?}", e.path());
            }
        }
        Err(_) => println!("Error!")
    }
}
