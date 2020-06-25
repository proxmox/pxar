use std::ffi::CString;
use std::path::PathBuf;

use anyhow::{bail, Error};

use pxar::decoder::sync as decoder;
use pxar::decoder::SeqRead;
use pxar::encoder::sync as encoder;
use pxar::encoder::{LinkOffset, SeqWrite};
use pxar::format::{self, mode};
use pxar::Metadata;

#[derive(Debug, Eq, PartialEq)]
pub enum EntryKind {
    Invalid,
    File(Vec<u8>),
    Directory(Vec<Entry>),
    Symlink(PathBuf),
    Hardlink, // TODO
    Device(format::Device),
    Socket,
    Fifo,
}

#[derive(Debug, Eq, PartialEq)]
pub struct Entry {
    pub name: String,
    pub metadata: Metadata,
    pub entry: EntryKind,
}

impl Entry {
    pub fn new(name: &str) -> Self {
        Self {
            name: name.to_string(),
            metadata: Metadata::default(),
            entry: EntryKind::Invalid,
        }
    }

    pub fn metadata(mut self, metadata: impl Into<Metadata>) -> Self {
        self.metadata = metadata.into();
        self
    }

    pub fn entry(mut self, entry: EntryKind) -> Self {
        self.entry = entry;
        self
    }

    pub fn encode_into<T>(&self, encoder: &mut encoder::Encoder<T>) -> Result<(), Error>
    where
        T: SeqWrite,
    {
        match &self.entry {
            EntryKind::Invalid => bail!("invalid entry encountered"),

            EntryKind::File(data) => {
                let _link: LinkOffset = encoder.add_file(
                    &self.metadata,
                    &self.name,
                    data.len() as u64,
                    &mut &data[..],
                )?;
            }

            EntryKind::Directory(entries) => {
                let mut dir = encoder.create_directory(&self.name, &self.metadata)?;
                for entry in entries {
                    entry.encode_into(&mut dir)?;
                }
                dir.finish()?;
            }

            EntryKind::Symlink(path) => {
                let _: () = encoder.add_symlink(&self.metadata, &self.name, path)?;
            }

            other => bail!("TODO: encode_entry for {:?}", other),
        }
        Ok(())
    }

    pub fn decode_from<T: SeqRead>(decoder: &mut decoder::Decoder<T>) -> Result<(), Error> {
        todo!();
    }
}

#[rustfmt::skip]
pub fn test_fs() -> Entry {
    Entry::new("/")
        .metadata(Metadata::dir_builder(0o755))
        .entry(EntryKind::Directory(vec![
            Entry::new("home")
                .metadata(Metadata::dir_builder(0o755))
                .entry(EntryKind::Directory(vec![
                    Entry::new("user")
                        .metadata(Metadata::dir_builder(0o700).owner(1000, 1000))
                        .entry(EntryKind::Directory(vec![
                            Entry::new(".profile")
                                .metadata(Metadata::file_builder(0o644).owner(1000, 1000))
                                .entry(EntryKind::File(b"#umask 022".to_vec())),
                            Entry::new("data")
                                .metadata(Metadata::file_builder(0o644).owner(1000, 1000))
                                .entry(EntryKind::File(b"a file from a user".to_vec())),
                        ])),
                ])),
            Entry::new("bin")
                .metadata(Metadata::builder(mode::IFLNK | 0o777))
                .entry(EntryKind::Symlink(PathBuf::from("usr/bin"))),
        ]))
}
