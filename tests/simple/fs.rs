use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{bail, format_err, Error};

use pxar::decoder::sync as decoder;
use pxar::decoder::SeqRead;
use pxar::encoder::sync as encoder;
use pxar::encoder::{LinkOffset, SeqWrite};
use pxar::format::{self, mode, Device};
use pxar::EntryKind as PxarEntryKind;
use pxar::Metadata;

/// Hardlink information we use while encoding pxar archives.
pub struct HardlinkInfo {
    link: LinkOffset,
    path: PathBuf,
}

/// Mapping from `Entry.link_key` to its produced `HardlinkInfo`.
pub type HardlinkList = HashMap<String, HardlinkInfo>;

/// Our virtual file system's file entry types.
#[derive(Debug, Eq, PartialEq)]
pub enum EntryKind {
    Invalid,
    File(Vec<u8>),
    Directory(Vec<Entry>),
    Symlink(PathBuf),
    Hardlink(String),
    Device(Device),
    Socket,
    Fifo,
}

/// A virtual file entry.
#[derive(Debug, Eq)]
pub struct Entry {
    /// For now we just use strings as those are easier to write down for test cases.
    pub name: String,

    /// The metadata is the same we have in pxar archives for ease of use.
    pub metadata: Metadata,

    /// The file's kind & contents.
    pub entry: EntryKind,

    /// If we want to make a hardlink to a file, we write a "key" in here by which we can refer
    /// back to it in an `EntryKind::Hardlink`. Note that in order for the tests to work we
    /// currently have to use the canonical file path as key. (This just made it much easier to
    /// test things by being able to just derive `PartialEq` on `EntryKind`.)
    pub link_key: Option<String>,
}

impl PartialEq for Entry {
    fn eq(&self, other: &Self) -> bool {
        // When decoding we don't get hardlink names back, so we skip those in our eq check.
        self.name == other.name && self.metadata == other.metadata && self.entry == other.entry
    }
}

impl Entry {
    /// Start a new entry. Takes a file *name* (not a path!). We use builder methods for the
    /// remaining data.
    pub fn new(name: &str) -> Self {
        Self {
            name: name.to_string(),
            metadata: Metadata::default(),
            entry: EntryKind::Invalid,
            link_key: None,
        }
    }

    /// Add metadata. You can skip this for hardlinks, but note that for other files you'll want to
    /// have metadat with the correct file mode bits at least.
    ///
    /// There could be convenience methods for files and symlinks though.
    pub fn metadata(mut self, metadata: impl Into<Metadata>) -> Self {
        self.metadata = metadata.into();
        self
    }

    /// Fill in the entry kind. This is never optional, actually, but since metadata is optional
    /// making this a builder method rather than a paramtere of `new()` makes the test cases read
    /// nicer, since directory contents are last.
    pub fn entry(mut self, entry: EntryKind) -> Self {
        self.entry = entry;
        self
    }

    /// NOTE: For test cases to be equal via the `==` comparison link keys must be equal to the full
    /// canonical path, as decoded hardlinks will use the `entry.path()` from the pxar decoder as
    /// value for `EntryKind::Hardlink`.
    pub fn link_key(mut self, key: impl Into<String>) -> Self {
        self.link_key = Some(key.into());
        self
    }

    /// Internal assertion that this file must not contain a hard link key.
    fn no_hardlink(&self) -> Result<(), Error> {
        if let Some(key) = &self.link_key {
            bail!(
                "hardlink key on entry which may not be hard-linked: {}",
                key
            );
        }
        Ok(())
    }

    /// Encode this entry (recursively) into a pxar encoder.
    ///
    /// The path should "point" to the parent.
    pub fn encode_into<T>(
        &self,
        encoder: &mut encoder::Encoder<T>,
        hardlinks: &mut HardlinkList,
        path: &Path,
    ) -> Result<(), Error>
    where
        T: SeqWrite,
    {
        match &self.entry {
            EntryKind::Invalid => bail!("invalid entry encountered"),

            EntryKind::File(data) => {
                let link: LinkOffset = encoder.add_file(
                    &self.metadata,
                    &self.name,
                    data.len() as u64,
                    &mut &data[..],
                )?;
                if let Some(key) = &self.link_key {
                    let info = HardlinkInfo {
                        link,
                        path: path.join(&self.name),
                    };
                    if hardlinks.insert(key.clone(), info).is_some() {
                        bail!("duplicate hardlink key: {}", key);
                    }
                }
            }

            EntryKind::Directory(entries) => {
                self.no_hardlink()?;
                let mut dir = encoder.create_directory(&self.name, &self.metadata)?;
                let path = path.join(&self.name);
                for entry in entries {
                    entry.encode_into(&mut dir, hardlinks, &path)?;
                }
                dir.finish()?;
            }

            EntryKind::Symlink(path) => {
                self.no_hardlink()?;
                let _: () = encoder.add_symlink(&self.metadata, &self.name, path)?;
            }

            EntryKind::Hardlink(key) => {
                self.no_hardlink()?;
                if let Some(info) = hardlinks.get(key) {
                    let _: () = encoder.add_hardlink(&self.name, &info.path, info.link)?;
                } else {
                    bail!("missing hardlink target key: {}", key);
                }
            }

            EntryKind::Device(device) => {
                self.no_hardlink()?;
                let _: () = encoder.add_device(&self.metadata, &self.name, device.clone())?;
            }

            EntryKind::Fifo => {
                self.no_hardlink()?;
                let _: () = encoder.add_fifo(&self.metadata, &self.name)?;
            }

            EntryKind::Socket => {
                self.no_hardlink()?;
                let _: () = encoder.add_socket(&self.metadata, &self.name)?;
            }

            other => bail!("TODO: encode_entry for {:?}", other),
        }
        Ok(())
    }

    /// Decode an entry from a pxar decoder.
    ///
    /// Note that you CANNOT re-encode it afterwards currently, as for now we do not synthesize
    /// `link_key` properties!
    pub fn decode_from<T: SeqRead>(decoder: &mut decoder::Decoder<T>) -> Result<Entry, Error> {
        // The decoder linearly goes through the entire archive recursiveley, we need to know when
        // a directory ends:
        decoder.enable_goodbye_entries(true);

        Self::decode_root(decoder)
    }

    fn decode_root<T: SeqRead>(decoder: &mut decoder::Decoder<T>) -> Result<Entry, Error> {
        let item = decoder
            .next()
            .ok_or_else(|| format_err!("empty archive?"))??;

        match item.kind() {
            PxarEntryKind::Directory => (),
            other => bail!("invalid entry kind for root node: {:?}", other),
        }

        let mut root = Entry::new("/").metadata(item.metadata().clone());
        root.decode_directory(decoder)?;
        Ok(root)
    }

    fn decode_directory<T: SeqRead>(
        &mut self,
        decoder: &mut decoder::Decoder<T>,
    ) -> Result<(), Error> {
        let mut contents = Vec::new();

        while let Some(item) = decoder.next().transpose()? {
            let make_entry =
                || -> Result<Entry, Error> {
                    Ok(Entry::new(item.file_name().to_str().ok_or_else(|| {
                        format_err!("non-utf8 name in test: {:?}", item.file_name())
                    })?)
                    .metadata(item.metadata().clone())
                    .link_key(item.path().as_os_str().to_str().ok_or_else(|| {
                        format_err!("non-utf8 path in test: {:?}", item.file_name())
                    })?))
                };
            match item.kind() {
                PxarEntryKind::GoodbyeTable => break,
                PxarEntryKind::File { size, .. } => {
                    let mut data = Vec::new();
                    decoder
                        .contents()
                        .ok_or_else(|| {
                            format_err!("failed to get contents for file entry: {:?}", item.path())
                        })?
                        .read_to_end(&mut data)?;
                    contents.push(make_entry()?.entry(EntryKind::File(data)));
                }
                PxarEntryKind::Directory => {
                    let mut dir = make_entry()?;
                    dir.decode_directory(decoder)?;
                    contents.push(dir);
                }
                PxarEntryKind::Symlink(link) => {
                    contents.push(make_entry()?.entry(EntryKind::Symlink(link.into())));
                }
                PxarEntryKind::Hardlink(link) => {
                    contents.push(
                        make_entry()?.entry(EntryKind::Hardlink(
                            link.as_os_str()
                                .to_str()
                                .ok_or_else(|| {
                                    format_err!("non-utf8 hardlink entry: {:?}", link.as_os_str())
                                })?
                                .to_string(),
                        )),
                    );
                }
                PxarEntryKind::Device(device) => {
                    contents.push(make_entry()?.entry(EntryKind::Device(device.clone())));
                }
                PxarEntryKind::Fifo => {
                    contents.push(make_entry()?.entry(EntryKind::Fifo));
                }
                PxarEntryKind::Socket => {
                    contents.push(make_entry()?.entry(EntryKind::Socket));
                }
                other => todo!("decode for kind {:?}", other),
            }
        }

        self.entry = EntryKind::Directory(contents);
        Ok(())
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
            Entry::new("usr")
                .metadata(Metadata::dir_builder(0o755))
                .entry(EntryKind::Directory(vec![
                    Entry::new("bin")
                        .metadata(Metadata::dir_builder(0o755))
                        .entry(EntryKind::Directory(vec![
                            Entry::new("bzip2")
                                .metadata(Metadata::file_builder(0o755))
                                .entry(EntryKind::File(b"This is an executable".to_vec()))
                                .link_key("/usr/bin/bzip2"),
                            Entry::new("cat")
                                .metadata(Metadata::file_builder(0o755))
                                .entry(EntryKind::File(b"This is another executable".to_vec())),
                            Entry::new("bunzip2")
                                .entry(EntryKind::Hardlink("/usr/bin/bzip2".to_string())),
                        ])),
                ])),
            Entry::new("dev")
                .metadata(Metadata::dir_builder(0o755))
                .entry(EntryKind::Directory(vec![
                    Entry::new("null")
                        .metadata(Metadata::builder(mode::IFCHR | 0o666))
                        .entry(EntryKind::Device(Device { major: 1, minor: 3 })),
                    Entry::new("zero")
                        .metadata(Metadata::builder(mode::IFCHR | 0o666))
                        .entry(EntryKind::Device(Device { major: 1, minor: 5 })),
                    Entry::new("loop0")
                        .metadata(Metadata::builder(mode::IFBLK | 0o666))
                        .entry(EntryKind::Device(Device { major: 7, minor: 0 })),
                    Entry::new("loop1")
                        .metadata(Metadata::builder(mode::IFBLK | 0o666))
                        .entry(EntryKind::Device(Device { major: 7, minor: 1 })),
                ])),
            Entry::new("run")
                .metadata(Metadata::dir_builder(0o755))
                .entry(EntryKind::Directory(vec![
                    Entry::new("fifo0")
                        .metadata(Metadata::builder(mode::IFIFO | 0o666))
                        .entry(EntryKind::Fifo),
                    Entry::new("sock0")
                        .metadata(Metadata::builder(mode::IFSOCK | 0o600))
                        .entry(EntryKind::Socket),
                ])),
        ]))
}
