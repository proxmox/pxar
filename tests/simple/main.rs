use anyhow::{bail, Error};

use pxar::decoder::sync as decoder;
use pxar::encoder::sync as encoder;
use pxar::encoder::{LinkOffset, SeqWrite};

mod fs;

fn encode_directory<T: SeqWrite>(
    encoder: &mut encoder::Encoder<T>,
    entry: &fs::Entry,
) -> Result<(), Error> {
    match &entry.entry {
        fs::EntryKind::Directory(entries) => {
            for entry in entries {
                entry.encode_into(encoder)?;
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
    encoder.finish();

    assert!(!file.is_empty(), "encoder did not write any data");

    let mut decoder = decoder::Decoder::from_std(&mut &file[..]).expect("failed to create decoder");
}
