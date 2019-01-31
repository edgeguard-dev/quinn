use bytes::Buf;
use quinn_h3::qpack;
use quinn_proto::coding::Codec;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::{fs, mem};
use structopt::StructOpt;

#[derive(StructOpt, Debug)]
#[structopt(name = "interop")]
struct Opt {
    path: String,
    #[structopt(short = "t", long = "table_size")]
    table_size: Option<String>,
    #[structopt(short = "b", long = "max_blocked")]
    max_blocked: Option<String>,
    #[structopt(short = "a", long = "ack_mode")]
    ack_mode: Option<String>,
}

fn main() -> Result<(), Error> {
    let opt = Opt::from_args();
    let input = &opt.path;

    let mut failures = vec![];
    let mut success = vec![];

    let table_size = if let Some(sizes) = opt.table_size {
        sizes
            .split(",")
            .map(|e| str::parse::<usize>(e).unwrap_or_default())
            .collect()
    } else {
        vec![4096usize, 512, 256, 0]
    };

    let max_blocked = if let Some(sizes) = opt.max_blocked {
        sizes
            .split(",")
            .map(|e| str::parse::<usize>(e).unwrap_or_default())
            .collect()
    } else {
        vec![100usize, 0]
    };

    let ack_mode = if let Some(sizes) = opt.ack_mode {
        sizes
            .split(",")
            .map(|e| str::parse::<usize>(e).unwrap_or_default())
            .collect()
    } else {
        vec![1usize, 0]
    };

    match InputType::what_is(Path::new(input))? {
        InputType::EncodedFile(file, qif) => match file.decode() {
            Err(e) => failures.push((file.file, e)),
            Ok(ref mut v) if qif.is_some() => {
                let mut fields = v.iter().flatten();
                qif.unwrap().compare(&mut fields)?;
                success.push(file.file);
            }
            Ok(_) => failures.push((file.file, Error::NoQif)),
        },
        InputType::ImplEncodedDir(dir) => {
            for (file, qif) in dir.iter()? {
                match file.decode() {
                    Err(e) => failures.push((file.file, e)),
                    Ok(ref mut v) if qif.is_some() => {
                        let mut fields = v.iter().flatten();
                        qif.unwrap().compare(&mut fields)?;
                        success.push(file.file);
                    }
                    Ok(_) => failures.push((file.file, Error::NoQif)),
                }
            }
        }
        InputType::EncodedDir(dir) => {
            for impl_dir in dir.iter()? {
                for (file, qif) in impl_dir.iter()? {
                    match file.decode() {
                        Err(e) => failures.push((file.file, e)),
                        Ok(ref mut v) if qif.is_some() => {
                            let mut fields = v.iter().flatten();
                            qif.unwrap().compare(&mut fields)?;
                            success.push(file.file);
                        }
                        Ok(_) => failures.push((file.file, Error::NoQif)),
                    }
                }
            }
        }
        InputType::QifFile(qif) => {
            for size in &table_size {
                for blocked in &max_blocked {
                    for ack in &ack_mode {
                        let enc_file = EncodedFile::from_qif(&qif.0, *size, *blocked, *ack)?;
                        match enc_file.encode(qif.blocks()?) {
                            Err(e) => failures.push((enc_file.file, e)),
                            Ok(_) => success.push(enc_file.file),
                        }
                    }
                }
            }
        }
        InputType::QifDir(qif_dir) => {
            for qif in qif_dir.iter()? {
                for size in &table_size {
                    for blocked in &max_blocked {
                        for ack in &ack_mode {
                            let enc_file = EncodedFile::from_qif(&qif.0, *size, *blocked, *ack)?;
                            match enc_file.encode(qif.blocks()?) {
                                Err(e) => failures.push((enc_file.file, e)),
                                Ok(_) => success.push(enc_file.file),
                            }
                        }
                    }
                }
            }
        }
        _ => unimplemented!(),
    }

    for failure in &failures {
        let mut name = failure
            .0
            .ancestors()
            .take(2)
            .map(|a| {
                a.file_name()
                    .unwrap_or(OsStr::new("ERR"))
                    .to_str()
                    .unwrap_or("ERR")
            })
            .map(|n| String::from(n))
            .collect::<Vec<String>>();

        name.reverse();
        let name = name.join("/");

        println!("{: <42}: {:?}", name, failure.1);
    }

    println!("\nSuccess: {}, Failures: {}", success.len(), failures.len());

    if failures.len() == 0 {
        Ok(())
    } else {
        Err(Error::SomeFailures)
    }
}

struct BlockIterator<R: Buf> {
    buf: R,
    last_block_len: usize,
}

impl<R: Buf> BlockIterator<R> {
    fn new(buf: R) -> Self {
        Self {
            buf,
            last_block_len: 0,
        }
    }

    fn next<'a>(&'a mut self) -> Result<Option<(std::io::Cursor<&'a [u8]>, u64)>, Error> {
        self.buf.advance(self.last_block_len);
        if self.buf.remaining() < mem::size_of::<u64>() + mem::size_of::<u32>() {
            if self.buf.remaining() > 0 {
                return Err(Error::TrailingData(self.buf.remaining()));
            }
            return Ok(None);
        }

        let current = u64::decode(&mut self.buf)?;
        let length = u32::decode(&mut self.buf)? as usize;

        if self.buf.remaining() < length {
            Err(Error::UnexpectedEnd)
        } else {
            self.last_block_len = length;
            let block = std::io::Cursor::new(&self.buf.bytes()[..length]);

            Ok(Some((block, current)))
        }
    }
}

#[derive(Debug)]
struct EncodedFile {
    file: PathBuf,
    table_size: usize,
    max_blocked: usize,
    ack_mode: usize,
}

impl EncodedFile {
    pub fn new(file: PathBuf) -> Self {
        let numbers = file
            .file_name()
            .unwrap_or_default()
            .to_str()
            .unwrap_or("")
            .rsplit('.')
            .take(3)
            .map(|n| str::parse::<usize>(n).unwrap_or_default())
            .collect::<Vec<_>>();

        let (ack_mode, max_blocked, table_size) = if numbers.len() >= 3 {
            (numbers[0], numbers[1], numbers[2])
        } else {
            (0, 0, 0)
        };

        Self {
            file,
            table_size,
            max_blocked,
            ack_mode,
        }
    }

    fn from_qif(
        qif: &Path,
        table_size: usize,
        max_blocked: usize,
        ack_mode: usize,
    ) -> Result<EncodedFile, Error> {
        let enc_dir = qif
            .ancestors()
            .skip(2)
            .take(1)
            .collect::<PathBuf>()
            .join("encoded")
            .join("qpack-05")
            .join("quinn");

        if !enc_dir.is_dir() {
            fs::create_dir(&enc_dir)?;
        }

        let name = qif.file_stem().ok_or(Error::BadFilename)?;
        let enc_file = enc_dir.join(format!(
            "{}.out.{}.{}.{}",
            name.to_str().ok_or(Error::BadFilename)?,
            table_size,
            max_blocked,
            ack_mode
        ));

        Ok(Self {
            file: enc_file,
            table_size,
            max_blocked,
            ack_mode,
        })
    }

    pub fn decode(&self) -> Result<Vec<Vec<qpack::HeaderField>>, Error> {
        let encoded = fs::read(&self.file)?;
        let mut table = qpack::DynamicTable::new();
        table.inserter().set_max_mem_size(self.table_size)?;
        let mut blocks = BlockIterator::new(std::io::Cursor::new(&encoded));
        let mut count = 0;
        let mut decoder = vec![];
        let mut decoded = vec![];

        while let Some((mut buf, current)) = blocks.next().expect("next block") {
            if current == 0 {
                // encoder stream
                qpack::on_encoder_recv(&mut table.inserter(), &mut buf, &mut decoder)?;
                continue;
            }

            if current != count + 1 {
                eprintln!("got wrong stream ID: {}", current);
                break;
            }

            decoded.push(qpack::decode_header(&mut table, &mut buf)?);
            count += 1;
        }
        Ok(decoded)
    }

    pub fn encode(&self, blocks: Vec<Vec<u8>>) -> Result<(), Error> {
        let mut table = qpack::DynamicTable::new();
        table.inserter().set_max_mem_size(self.table_size)?;

        let mut buf = vec![];
        let mut stream_count = 1;

        for block in blocks {
            let block_str = String::from_utf8_lossy(&block);
            let fields = block_str
                .split("\n")
                .map(|f| f.split("\t"))
                .filter_map(|s| {
                    let f = s.collect::<Vec<&str>>();
                    if f.len() >= 2 {
                        Some((f[0], f[1]))
                    } else {
                        None
                    }
                })
                .map(|(k, v)| qpack::HeaderField::new(k, v))
                .collect::<Vec<_>>();

            let mut block_chunk = vec![];
            let mut encoder_chunk = vec![];

            qpack::encode(
                &mut table.encoder(stream_count),
                &mut block_chunk,
                &mut encoder_chunk,
                &fields,
            )?;

            if self.ack_mode == 1 && self.table_size > 0 {
                let mut decoder_stream = vec![];
                qpack::ack_header(stream_count, &mut decoder_stream);
                let mut cur = std::io::Cursor::new(&decoder_stream);
                qpack::on_decoder_recv(&mut table, &mut cur)?;
            }

            if encoder_chunk.len() > 0 {
                0u64.encode(&mut buf);
                (encoder_chunk.len() as u32).encode(&mut buf);
                buf.append(&mut encoder_chunk);
            }

            stream_count.encode(&mut buf);
            (block_chunk.len() as u32).encode(&mut buf);
            buf.append(&mut block_chunk);

            stream_count += 1;
        }
        fs::write(&self.file, buf)?;
        Ok(())
    }
}

struct ImplEncodedDir(PathBuf, String);

impl ImplEncodedDir {
    pub fn iter(&self) -> Result<impl Iterator<Item = (EncodedFile, Option<QifFile>)>, Error> {
        Ok(self
            .0
            .read_dir()?
            .filter_map(|e| e.ok())
            .filter(|e| e.path().is_file())
            .map(|e| {
                (
                    EncodedFile::new(e.path()),
                    find_qif(&Path::new(&e.path())).unwrap_or(None),
                )
            }))
    }
}

struct EncodedDir(PathBuf);

impl EncodedDir {
    pub fn iter(&self) -> Result<impl Iterator<Item = ImplEncodedDir>, Error> {
        let path = if self.0.file_name().unwrap_or_default() == OsStr::new("encoded") {
            self.0.join("qpack-05")
        } else {
            self.0.clone()
        };

        Ok(path
            .read_dir()?
            .filter_map(|e| e.ok())
            .filter(|e| e.path().is_dir())
            .map(|e| {
                ImplEncodedDir(
                    e.path(),
                    e.file_name().to_str().unwrap_or_default().to_string(),
                )
            }))
    }
}

struct QifFile(PathBuf);

impl QifFile {
    fn compare(&self, other: &mut Iterator<Item = &qpack::HeaderField>) -> Result<(), Error> {
        let value = String::from_utf8_lossy(&fs::read(&self.0)?)
            .split("\n")
            .filter(|l| l.len() != 0)
            .map(|c| Vec::from(c))
            .collect::<Vec<Vec<u8>>>();

        for field in value.iter() {
            let field_str = String::from_utf8_lossy(&field.to_owned()).to_string();
            let other_field = other.next().ok_or(Error::MissingDecoded)?;

            if field_str != other_field.to_string() {
                return Err(Error::NotMatching(field_str, other_field.to_string()));
            }
        }

        Ok(())
    }

    fn blocks(&self) -> Result<Vec<Vec<u8>>, Error> {
        let content = fs::read_to_string(&self.0)?;

        let blocks = content
            .split("\n\n")
            .filter(|l| l.len() != 0)
            .map(|c| Vec::from(c))
            .collect::<Vec<Vec<u8>>>();
        Ok(blocks)
    }
}

struct QifDir(PathBuf);

impl QifDir {
    pub fn iter(&self) -> Result<impl Iterator<Item = QifFile>, Error> {
        Ok(self
            .0
            .read_dir()?
            .filter_map(|e| e.ok())
            .filter(|e| e.path().is_file() && e.path().extension() == Some(OsStr::new("qif")))
            .map(|f| QifFile(f.path())))
    }
}

enum InputType {
    EncodedFile(EncodedFile, Option<QifFile>),
    ImplEncodedDir(ImplEncodedDir),
    EncodedDir(EncodedDir),
    QifFile(QifFile),
    QifDir(QifDir),
    Unknown,
}

impl InputType {
    fn is_impl_encoded_dir(path: &Path) -> bool {
        if !path.is_dir() {
            return false;
        }
        let ancestors = path
            .ancestors()
            .map(|e| e.file_name())
            .filter_map(|e| e)
            .take(3)
            .collect::<Vec<_>>();

        if ancestors.len() >= 3
            && ancestors[1] == OsStr::new("qpack-05")
            && ancestors[2] == OsStr::new("encoded")
        {
            true
        } else {
            false
        }
    }

    fn is_encoded_dir(path: &Path) -> Result<bool, Error> {
        if !path.is_dir() {
            return Ok(false);
        }

        Ok(
            path.file_name().unwrap_or_default() == OsStr::new("encoded")
                || path.file_name().unwrap_or_default() == OsStr::new("qpack-05"),
        )
    }

    fn is_qif_dir(path: &Path) -> Result<bool, Error> {
        if !path.is_dir() {
            return Ok(false);
        }

        Ok(path
            .read_dir()?
            .filter_map(|e| e.ok())
            .any(|f| f.path().extension() == Some(OsStr::new("qif"))))
    }

    pub fn what_is(path: &Path) -> Result<Self, Error> {
        let input_type = if path.is_file() {
            let file_name = path
                .file_name()
                .ok_or(Error::BadFilename)?
                .to_str()
                .ok_or(Error::BadFilename)?;
            if file_name.contains(".out") {
                InputType::EncodedFile(
                    EncodedFile::new(PathBuf::from(path)),
                    find_qif(&Path::new(path)).unwrap_or(None),
                )
            } else if file_name.ends_with(".qif") {
                InputType::QifFile(QifFile(PathBuf::from(path)))
            } else {
                InputType::Unknown
            }
        } else if InputType::is_impl_encoded_dir(path) {
            InputType::ImplEncodedDir(ImplEncodedDir(
                path.to_path_buf(),
                path.parent()
                    .unwrap()
                    .file_name()
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .into(),
            ))
        } else if InputType::is_encoded_dir(path)? {
            println!("encoder dir");
            InputType::EncodedDir(EncodedDir(path.to_path_buf()))
        } else if InputType::is_qif_dir(path)? {
            InputType::QifDir(QifDir(path.to_path_buf()))
        } else {
            InputType::Unknown
        };
        Ok(input_type)
    }
}

fn find_qif(path: &Path) -> Result<Option<QifFile>, Error> {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .map(|s| s.split(".").take(1).collect::<String>())
        .ok_or(Error::BadFilename)?;

    Ok(find_qif_dir(path)?.map(|d| QifFile(d.join(name + ".qif"))))
}

fn find_qif_dir(path: &Path) -> Result<Option<PathBuf>, std::io::Error> {
    let ancestors = path.ancestors().skip(1).take(5).collect::<Vec<_>>();
    for a in ancestors {
        let mut dir = a.read_dir()?;
        let found = dir.find(|ref mut e| {
            if let Ok(e) = e {
                e.path().file_name() == Some(OsStr::new("qifs"))
            } else {
                false
            }
        });

        if found.is_some() {
            return Ok(Some(found.unwrap()?.path()));
        }
    }
    Ok(None)
}

#[derive(Debug)]
enum Error {
    TrailingData(usize),
    UnexpectedEnd,
    BadFilename,
    IO(std::io::Error),
    Decode(qpack::DecoderError),
    DynamicTable(qpack::DynamicTableError),
    NoQif,
    MissingDecoded,
    NotMatching(String, String),
    SomeFailures,
    Encode(qpack::EncoderError),
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Error {
        Error::IO(e)
    }
}

impl From<qpack::DecoderError> for Error {
    fn from(e: qpack::DecoderError) -> Error {
        Error::Decode(e)
    }
}

impl From<qpack::EncoderError> for Error {
    fn from(e: qpack::EncoderError) -> Error {
        Error::Encode(e)
    }
}

impl From<quinn_proto::coding::UnexpectedEnd> for Error {
    fn from(_: quinn_proto::coding::UnexpectedEnd) -> Error {
        Error::UnexpectedEnd
    }
}

impl From<qpack::DynamicTableError> for Error {
    fn from(e: qpack::DynamicTableError) -> Error {
        Error::DynamicTable(e)
    }
}