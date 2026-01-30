use std::fs;
use std::io;
use std::path::Path;
use regex::Regex;

pub fn search(query: &str, directory: &str) -> io::Result<()> {
    let path = Path::new(directory);

    if path.is_dir() {
        for entry in fs::read_dir(path)? {
            let entry = entry?; // Handle potential errors
            let path = entry.path();

            if path.is_file() {
                if let Some(extension) = path.extension() {
                    if extension == "rs" {
                        let file = fs::File::open(path)?; // Open the file
                        let reader = io::BufReader::new(file);

                        for line in io::BufRead::lines(reader) {
                            let line = line?;

                            if let Ok(_) = Regex::new(query).unwrap().is_match(&line) {
                                println!("{}: {}", path.display(), line);
                            }
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_search() {
        // Add tests here later!
    }
}
