use std::io::{self, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::sync::Arc;

use failure::{bail, format_err, Error};

use pxar::accessor::Accessor;

fn main() -> Result<(), Error> {
    let mut args = std::env::args_os().skip(1);

    let cmd = args
        .next()
        .ok_or_else(|| format_err!("expected a command (ls or cat)"))?;
    let cmd = cmd
        .to_str()
        .ok_or_else(|| format_err!("expected a valid command string (utf-8)"))?;
    match cmd {
        "ls" | "cat" => (),
        _ => bail!("valid commands are: cat, ls"),
    }

    let file = args
        .next()
        .ok_or_else(|| format_err!("expected a file name"))?;
    let mut accessor = Accessor::open(file)?;
    let mut dir = accessor.open_root_ref()?;

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
