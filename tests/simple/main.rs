use std::path::Path;

use anyhow::{bail, Error};

use pxar::decoder::sync as decoder;
use pxar::encoder::sync as encoder;
use pxar::encoder::SeqWrite;

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
}
