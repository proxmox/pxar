//! Test for old timestamp format compatibility.

use std::io::{self, Read};

use endian_trait::Endian;

use pxar::{decoder, format, EntryKind};

fn write_raw_struct<T: Endian, W: io::Write + ?Sized>(output: &mut W, data: T) -> io::Result<()> {
    let data = data.to_le();
    output.write_all(unsafe {
        std::slice::from_raw_parts(&data as *const T as *const u8, std::mem::size_of::<T>())
    })
}

fn write_data<W>(output: &mut W, htype: u64, data: &[u8]) -> io::Result<()>
where
    W: io::Write + ?Sized,
{
    let header = format::Header::with_content_size(htype, data.len() as u64);
    header.check_header_size()?;
    write_raw_struct(output, header)?;
    output.write_all(data)
}

fn write_entry<T, W>(output: &mut W, htype: u64, data: T) -> io::Result<()>
where
    T: Endian,
    W: io::Write + ?Sized,
{
    let data = data.to_le();
    let data = unsafe {
        std::slice::from_raw_parts(&data as *const T as *const u8, std::mem::size_of::<T>())
    };
    write_data(output, htype, data)
}

const MAY_1_2015_1530: u64 = 1430487000u64;

const FILE_NAME: &str = "file.txt";
const FILE_NAME_BYTES: &[u8] = b"file.txt\0";
const FILE_CONTENT: &[u8] = b"This is a small text file.\n";
const ROOT_STAT: format::Stat_V1 = format::Stat_V1 {
    mode: format::mode::IFDIR | 0o755,
    flags: 0,
    uid: 1000,
    gid: 1000,
    mtime: MAY_1_2015_1530 * 1_000_000_000u64,
};
const FILE_STAT: format::Stat_V1 = format::Stat_V1 {
    mode: format::mode::IFREG | 0o644,
    flags: 0,
    uid: 1000,
    gid: 1000,
    mtime: MAY_1_2015_1530 * 1_000_000_000u64,
};

fn create_archive() -> io::Result<Vec<u8>> {
    let mut out = Vec::new();

    write_entry(&mut out, format::PXAR_ENTRY_V1, ROOT_STAT.clone())?;

    let file_offset = out.len();
    write_data(&mut out, format::PXAR_FILENAME, FILE_NAME_BYTES)?;
    write_entry(&mut out, format::PXAR_ENTRY_V1, FILE_STAT.clone())?;
    write_data(&mut out, format::PXAR_PAYLOAD, FILE_CONTENT)?;

    let mut gbt = Vec::new();
    write_raw_struct(
        &mut gbt,
        format::GoodbyeItem::new(
            FILE_NAME.as_bytes(),
            file_offset as u64,
            FILE_CONTENT.len() as u64,
        ),
    )?;

    let gbt_size = gbt.len();
    write_raw_struct(
        &mut gbt,
        format::GoodbyeItem {
            hash: format::PXAR_GOODBYE_TAIL_MARKER,
            offset: out.len() as u64,
            size: gbt_size as u64,
        },
    )?;

    write_data(&mut out, format::PXAR_GOODBYE, &{ gbt })?;

    Ok(out)
}

#[test]
fn test_archive() {
    let archive = create_archive().expect("failed to create test archive");
    let mut input = &archive[..];
    let mut decoder = decoder::Decoder::from_std(&mut input).expect("failed to create decoder");

    let item = decoder
        .next()
        .expect("missing root directory in test archive")
        .expect("failed to extract root directory from test archive");
    match item.kind() {
        EntryKind::Directory => (),
        other => panic!("unexpected root entry in archive: {:?}", other),
    }
    assert_eq!(item.file_name(), "");
    assert_eq!(item.metadata().stat, ROOT_STAT.into());
    assert_eq!(
        item.metadata().stat.mtime,
        format::StatxTimestamp {
            secs: MAY_1_2015_1530 as i64,
            nanos: 0,
        },
    );

    let item = decoder
        .next()
        .expect("missing file entry in test archive")
        .expect("failed to extract file entry from test archive");
    match item.kind() {
        EntryKind::File { size, .. } => assert_eq!(*size, FILE_CONTENT.len() as u64),
        other => panic!("unexpected file entry in archive: {:?}", other),
    }
    assert_eq!(item.file_name(), FILE_NAME);
    assert_eq!(item.metadata().stat, FILE_STAT.into());
    let mut content = Vec::new();
    decoder
        .contents()
        .expect("failed to get contents for file entry")
        .read_to_end(&mut content)
        .expect("failed to read test file contents");
    assert_eq!(&content[..], FILE_CONTENT);

    assert!(decoder.next().is_none(), "expected end of test archive");
}
