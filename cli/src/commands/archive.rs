use std::fs;
use std::io::{self, Write};
use std::path::Path;

use oak_core::{IgnorePatterns, Result};
use zip::write::SimpleFileOptions;
use zip::CompressionMethod;

use crate::output;

fn zip_err(e: zip::result::ZipError) -> io::Error {
    io::Error::other(e)
}

/// Create a zip archive of the current working directory
pub fn run(path: &Path, output_path: Option<&Path>) -> Result<()> {
    let ignore = IgnorePatterns::new(path)?;

    // Determine output file path
    let zip_path = match output_path {
        Some(p) => p.to_path_buf(),
        None => {
            let dir_name = path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| "archive".to_string());
            path.join(format!("{dir_name}.zip"))
        }
    };

    let file = fs::File::create(&zip_path)?;
    let mut zip = zip::ZipWriter::new(file);
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);

    let mut count = 0;
    walk_and_zip(
        &mut zip, path, path, &zip_path, &ignore, &options, &mut count,
    )?;

    zip.finish().map_err(zip_err)?;

    output::success(&format!(
        "Archived {} file(s) to {}",
        count,
        zip_path.display()
    ));

    Ok(())
}

fn walk_and_zip<W: Write + io::Seek>(
    zip: &mut zip::ZipWriter<W>,
    dir: &Path,
    root: &Path,
    output_path: &Path,
    ignore: &IgnorePatterns,
    options: &SimpleFileOptions,
    count: &mut usize,
) -> Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let relative_path = path.strip_prefix(root).unwrap();
        let file_type = entry.file_type()?;
        let is_dir = file_type.is_dir();

        if same_path(&path, output_path) {
            continue;
        }

        if ignore.is_ignored(relative_path, is_dir) {
            continue;
        }

        if is_dir {
            walk_and_zip(zip, &path, root, output_path, ignore, options, count)?;
        } else if file_type.is_file() {
            let name = relative_path.to_string_lossy();
            zip.start_file(name, *options).map_err(zip_err)?;
            let content = fs::read(&path)?;
            zip.write_all(&content)?;
            *count += 1;
        }
    }
    Ok(())
}

fn same_path(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    match (fs::canonicalize(a), fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}
