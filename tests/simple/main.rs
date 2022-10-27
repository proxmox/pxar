use std::io::Read;
use std::path::Path;

use pxar::accessor::sync as accessor;
use pxar::decoder::sync as decoder;
use pxar::encoder::sync as encoder;
use pxar::encoder::SeqWrite;
use pxar::EntryKind as PxarEntryKind;

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

pub type Error = Box<dyn std::error::Error + Send + Sync + 'static>;

mod fs;

fn encode_directory<T: SeqWrite>(
    encoder: &mut encoder::Encoder<T>,
    entry: &fs::Entry,
) -> Result<(), Error> {
    let mut hardlinks = fs::HardlinkList::new();

    match &entry.entry {
        fs::EntryKind::Directory(entries) => {
            for entry in entries {
                entry.encode_into(encoder, &mut hardlinks, Path::new("/"))?;
            }
            Ok(())
        }
        _ => bail!("encode_directory on a non-directory"),
    }
}

#[test]
fn test1() {
    let mut file = Vec::<u8>::new();

    let test_fs = fs::test_fs();
    let mut encoder =
        encoder::Encoder::from_std(&mut file, &test_fs.metadata).expect("failed to create encoder");
    encode_directory(&mut encoder, &test_fs).expect("failed to encode test file system");
    encoder
        .finish()
        .expect("failed to finish encoding the pxar archive");

    assert!(!file.is_empty(), "encoder did not write any data");

    // may be useful for testing...
    // std::fs::write("myarchive.pxar", &file).expect("failed to write out test archive");

    let mut input = &file[..];
    let mut decoder = decoder::Decoder::from_std(&mut input).expect("failed to create decoder");
    let decoded_fs =
        fs::Entry::decode_from(&mut decoder).expect("failed to decode previously encoded archive");

    assert_eq!(test_fs, decoded_fs);

    let accessor = accessor::Accessor::new(&file[..], file.len() as u64)
        .expect("failed to create random access reader for encoded archive");

    check_bunzip2(&accessor);
    check_run_special_files(&accessor);
}

fn check_bunzip2(accessor: &accessor::Accessor<&[u8]>) {
    let root = accessor
        .open_root()
        .expect("failed to open root of encoded archive");

    let bunzip2 = root
        .lookup("usr/bin/bunzip2")
        .expect("failed to lookup usr/bin/bunzip2 in test data")
        .expect("missing usr/bin/bunzip2 in test data");
    match bunzip2.kind() {
        PxarEntryKind::Hardlink(link) => assert_eq!(link.as_os_str(), "/usr/bin/bzip2"),
        _ => panic!("expected usr/bin/bunzip2 in test data to be a hardlink"),
    }
    let bzip2 = accessor
        .follow_hardlink(&bunzip2)
        .expect("failed to follow usr/bin/bunzip2 hardlink in test data");
    assert!(bzip2.is_regular_file());
    let mut content = String::new();
    let len = bzip2
        .contents()
        .expect("failed to get contents of bzip2 file")
        .read_to_string(&mut content)
        .expect("failed to read bzip2 file into a string");
    match bzip2.kind() {
        PxarEntryKind::File { size, .. } => assert_eq!(len as u64, *size),
        _ => panic!("expected usr/bin/bzip2 in test data to be a regular file"),
    }

    assert_eq!(content, "This is the bzip2 executable");
}

fn check_run_special_files(accessor: &accessor::Accessor<&[u8]>) {
    let rundir = accessor
        .open_root()
        .expect("failed to open root of encoded archive")
        .lookup("run")
        .expect("failed to open /run in encoded archive")
        .expect("missing /run in encoded archive")
        .enter_directory()
        .expect("expected /run to be a directory in the test archive");

    assert_eq!(rundir.entry_count(), 2, "expected 2 entries in /run");

    let mut rd = rundir.read_dir();
    let fifo0 = rd
        .next()
        .expect("expected 'fifo0' entry in rundir")
        .expect("failed to get first (fifo0) entry in test archive /run directory");
    assert_eq!(
        fifo0.file_name(),
        Path::new("fifo0"),
        "expected first file in /run to be fifo0"
    );

    let entry = fifo0
        .decode_entry()
        .expect("failed to decode entry for fifo0");
    assert!(matches!(entry.kind(), PxarEntryKind::Fifo));

    let sock0 = rd
        .next()
        .expect("expected 'sock0' entry in rundir")
        .expect("failed to get second (sock0) entry in test archive /run directory");
    assert_eq!(
        sock0.file_name(),
        Path::new("sock0"),
        "expected second file in /run to be sock0"
    );

    let entry = sock0
        .decode_entry()
        .expect("failed to decode entry for sock0");
    assert!(matches!(entry.kind(), PxarEntryKind::Socket));
}
