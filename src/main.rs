use std::fs;
use std::io;

fn is_file(entry: &fs::DirEntry) -> bool {
    entry.metadata().ok()
        .map(|m| m.is_file())
        .unwrap_or(false)
}

fn into_file(entry: io::Result<fs::DirEntry>) -> Option<fs::DirEntry> {
    entry.ok().and_then(|e| {
        if is_file(&e) {
            Some(e)
        } else {
            None
        }
    })
}

fn list_files(dir: &str) -> Result<(), Box<std::error::Error>>{
    let dir = fs::read_dir(dir)?;
    let files = dir.filter_map(into_file);
    for file in files {
        println!("{:?}", file.path());
    }
    Ok(())
}

fn main() {
    list_files("./").expect("Error!")
}
