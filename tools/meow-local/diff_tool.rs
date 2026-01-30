use diff::Diff;
use std::fs;
use std::io;

fn diff_files(file1_path: &str, file2_path: &str) -> Result<String, io::Error> {
    let file1_content = fs::read_to_string(file1_path)?;
    let file2_content = fs::read_to_string(file2_path)?;

    let diff = Diff::diff(&file1_content, &file2_content);

    let unified_diff = diff.unified_diff();

    Ok(unified_diff)
}

fn main() {
    let file1 = "file1.txt";
    let file2 = "file2.txt";

    match diff_files(file1, file2) {
        Ok(diff_output) => {
            println!("{}", diff_output);
        }
        Err(e) => {
            eprintln!("Error: {}", e);
        }
    }
}
