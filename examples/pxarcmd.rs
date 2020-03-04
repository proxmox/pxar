use std::fs::File;
use std::io::{self, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;

use failure::{bail, format_err, Error};

use pxar::accessor::Accessor;
use pxar::encoder::{Encoder, SeqWrite};
use pxar::{Metadata, Stat};

fn main() -> Result<(), Error> {
    let mut args = std::env::args_os();
    let _ = args.next();

    let cmd = args
        .next()
        .ok_or_else(|| format_err!("expected a command (ls or cat)"))?;
    let cmd = cmd
        .to_str()
        .ok_or_else(|| format_err!("expected a valid command string (utf-8)"))?;
    match cmd {
        "create" => return cmd_create(args),
        "ls" | "cat" => (),
        _ => bail!("valid commands are: cat, ls"),
    }

    let file = args
        .next()
        .ok_or_else(|| format_err!("expected a file name"))?;
    let accessor = Accessor::open(file)?;
    let dir = accessor.open_root_ref()?;

    let mut buf = Vec::new();

    for file in args {
        let entry = dir
            .lookup(&file)?
            .ok_or_else(|| format_err!("no such file in archive: {:?}", file))?;

        if cmd == "ls" {
            if file.as_bytes().ends_with(b"/") {
                for file in entry.enter_directory()?.read_dir() {
                    println!("{:?}", file?.file_name());
                }
            } else {
                println!("{:?}", entry.metadata());
            }
        } else if cmd == "cat" {
            buf.clear();
            entry.contents()?.read_to_end(&mut buf)?;
            io::stdout().write_all(&buf)?;
        } else {
            bail!("unknown command: {}", cmd);
        }
    }

    Ok(())
}

fn cmd_create(mut args: std::env::ArgsOs) -> Result<(), Error> {
    let file = args
        .next()
        .ok_or_else(|| format_err!("expected a file name"))?;

    let dir_path = args
        .next()
        .ok_or_else(|| format_err!("expected a directory"))?;

    if args.next().is_some() {
        bail!("too many parameters, there can only be a single root directory in a pxar archive");
    }

    // we use "simple" directory traversal without `openat()`
    let meta = Metadata::from(std::fs::metadata(&dir_path)?);
    let dir = std::fs::read_dir(dir_path)?;

    let mut encoder = Encoder::create(file, &meta)?;
    add_directory(&mut encoder, dir)?;
    encoder.finish();

    Ok(())
}

fn open_file<P: AsRef<Path>>(path: P) -> io::Result<(File, u64, Metadata)> {
    let file = File::open(path.as_ref())?;
    let meta = file.metadata()?;
    let file_size = meta.len();
    Ok((file, file_size, meta.into()))
}

fn add_directory<'a, T: SeqWrite + 'a>(
    encoder: &mut Encoder<T>,
    dir: std::fs::ReadDir,
) -> Result<(), Error> {
    for file in dir {
        let file = file?;
        let file_name = file.file_name();
        if file_name == "." || file_name == ".." {
            continue;
        }

        println!("{:?}", file.path());

        let file_type = file.file_type()?;
        let file_meta = file.metadata()?;
        let meta = Metadata::from(&file_meta);
        if file_type.is_dir() {
            let mut dir = encoder.create_directory(file_name, &meta)?;
            add_directory(&mut dir, std::fs::read_dir(file.path())?)?;
            dir.finish();
        } else if file_type.is_symlink() {
            todo!("symlink handling");
        } else if file_type.is_file() {
            encoder.add_file(
                &meta,
                file_name,
                file_meta.len(),
                &mut File::open(file.path())?,
            )?;
        } else {
            todo!("special file handling");
        }
    }
    Ok(())
}
