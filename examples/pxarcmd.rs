use std::fs::File;
use std::io::{self, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;

use failure::{bail, format_err, Error};

use pxar::accessor::Accessor;
use pxar::encoder::Encoder;
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

    let mut encoder = Encoder::create(
        file,
        &Stat::default()
            .mode(0o644)
            .set_dir()
            .uid(1000)
            .gid(1000)
            .into(),
    )?;

    for path in args {
        let (mut file, file_size, meta) = open_file(&path)?;
        encoder.add_file(&meta, &path, file_size, &mut file)?;
    }

    encoder.finish();

    Ok(())
}

fn open_file<P: AsRef<Path>>(path: P) -> io::Result<(File, u64, Metadata)> {
    let file = File::open(path.as_ref())?;
    let meta = file.metadata()?;
    let file_size = meta.len();
    Ok((file, file_size, meta.into()))
}
