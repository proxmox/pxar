use std::collections::HashMap;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::linux::fs::MetadataExt;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use pxar::accessor::Accessor;
use pxar::encoder::{Encoder, LinkOffset, SeqWrite};
use pxar::Metadata;

macro_rules! format_err {
    ($($msg:tt)+) => {
        std::io::Error::new(std::io::ErrorKind::Other, format!($($msg)+))
    };
}

macro_rules! bail {
    ($($msg:tt)+) => {{
        return Err(format_err!($($msg)+).into());
    }};
}

type Error = Box<dyn std::error::Error + Send + Sync + 'static>;

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
    let file = std::fs::File::open(file)?;
    let accessor = Accessor::from_file_ref(&file)?;
    let dir = accessor.open_root()?;

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
            let entry = if entry.is_hardlink() {
                accessor.follow_hardlink(&entry)?
            } else {
                entry
            };
            entry.contents()?.read_to_end(&mut buf)?;
            io::stdout().write_all(&buf)?;
        } else {
            bail!("unknown command: {}", cmd);
        }
    }

    Ok(())
}

#[derive(Eq, PartialEq, Hash)]
struct HardLinkInfo {
    st_dev: u64,
    st_ino: u64,
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

    let dir_path = Path::new(&dir_path);

    // we use "simple" directory traversal without `openat()`
    let meta = Metadata::from(std::fs::metadata(&dir_path)?);
    let dir = std::fs::read_dir(&dir_path)?;

    let mut encoder = Encoder::create(file, &meta)?;
    add_directory(&mut encoder, dir, &dir_path, &mut HashMap::new())?;
    encoder.finish()?;
    encoder.close()?;

    Ok(())
}

fn add_directory<'a, T: SeqWrite + 'a>(
    encoder: &mut Encoder<T>,
    dir: std::fs::ReadDir,
    root_path: &Path,
    hardlinks: &mut HashMap<HardLinkInfo, (PathBuf, LinkOffset)>,
) -> Result<(), Error> {
    let mut file_list = Vec::new();
    for file in dir {
        let file = file?;
        let file_name = file.file_name();
        if file_name == "." || file_name == ".." {
            continue;
        }
        file_list.push((
            file_name.to_os_string(),
            file.path().to_path_buf(),
            file.file_type()?,
            file.metadata()?,
        ));
    }

    file_list.sort_unstable_by(|a, b| (a.0).cmp(&b.0));

    for (file_name, file_path, file_type, file_meta) in file_list {
        println!("{:?}", file_path);

        let meta = Metadata::from(&file_meta);
        if file_type.is_dir() {
            encoder.create_directory(file_name, &meta)?;
            add_directory(
                encoder,
                std::fs::read_dir(file_path)?,
                root_path,
                &mut *hardlinks,
            )?;
            encoder.finish()?;
        } else if file_type.is_symlink() {
            todo!("symlink handling");
        } else if file_type.is_file() {
            let link_info = HardLinkInfo {
                st_dev: file_meta.st_dev(),
                st_ino: file_meta.st_ino(),
            };

            if file_meta.st_nlink() > 1 {
                if let Some((path, offset)) = hardlinks.get(&link_info) {
                    eprintln!("Adding hardlink {:?} => {:?}", file_name, path);
                    encoder.add_hardlink(file_name, path, *offset)?;
                    continue;
                }
            }

            let offset: LinkOffset = encoder.add_file(
                &meta,
                file_name,
                file_meta.len(),
                &mut File::open(&file_path)?,
            )?;

            if file_meta.st_nlink() > 1 {
                let file_path = file_path.strip_prefix(root_path)?.to_path_buf();
                hardlinks.insert(link_info, (file_path, offset));
            }
        } else {
            todo!("special file handling");
        }
    }
    Ok(())
}
