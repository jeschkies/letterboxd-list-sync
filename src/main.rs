use std::fs;

fn main() {
    match fs::read_dir("./") {
        Ok(entries) => {
            for entry in entries {
                let e = entry.unwrap();
                if let Ok(metadata) = e.metadata() {
                    if metadata.is_file() {
                        println!("{:?}", e.path());
                    }
                }
            }
        }
        Err(e) => println!("Error!")
    }
}
