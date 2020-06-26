use std::io::Read;
use std::path::Path;

use anyhow::{bail, Error};

use pxar::accessor::sync as accessor;
use pxar::decoder::sync as decoder;
use pxar::encoder::sync as encoder;
use pxar::encoder::SeqWrite;
use pxar::EntryKind as PxarEntryKind;

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

    let mut input = &file[..];
    let mut decoder = decoder::Decoder::from_std(&mut input).expect("failed to create decoder");
    let decoded_fs =
        fs::Entry::decode_from(&mut decoder).expect("failed to decode previously encoded archive");

    assert_eq!(test_fs, decoded_fs);

    let accessor = accessor::Accessor::new(&file[..], file.len() as u64)
        .expect("failed to create random access reader for encoded archive");

    check_bunzip2(&accessor);
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
