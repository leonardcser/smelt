//! Seekable provider blocks. Only the small directory is loaded on open;
//! lookups stream one provider and deserialize only the requested model.

use std::io::{self, BufReader, Read, Seek, SeekFrom, Write};
use std::sync::Mutex;

use serde::de::{DeserializeSeed, IgnoredAny, MapAccess, Visitor};
use serde::{Deserialize, Serialize};

use super::compiler::{compile_provider, Provider};
use super::{api_key, model_slug, ModelEntry};

const MAGIC: &[u8; 8] = b"SMCAT001";
const FOOTER_LEN: u64 = 16;
const MAX_DIRECTORY_LEN: u64 = 1024 * 1024;

trait ReadSeek: Read + Seek {}
impl<T: Read + Seek> ReadSeek for T {}

#[derive(Serialize, Deserialize)]
struct ProviderBlock {
    id: String,
    api: Option<String>,
    offset: u64,
    len: u64,
}

#[derive(Serialize, Deserialize)]
struct Directory {
    expires: u64,
    providers: Vec<ProviderBlock>,
}

pub(super) struct Archive {
    reader: Mutex<Box<dyn ReadSeek + Send>>,
    directory: Directory,
}

fn invalid(message: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.to_string())
}

impl Archive {
    pub(super) fn open(reader: impl Read + Seek + Send + 'static) -> io::Result<Self> {
        let mut reader = BufReader::new(reader);
        let size = reader.seek(SeekFrom::End(0))?;
        if size < FOOTER_LEN {
            return Err(invalid("missing catalog footer"));
        }
        reader.seek(SeekFrom::End(-(FOOTER_LEN as i64)))?;
        let mut footer = [0; FOOTER_LEN as usize];
        reader.read_exact(&mut footer)?;
        if &footer[8..] != MAGIC {
            return Err(invalid("unknown catalog format"));
        }
        let len = u64::from_le_bytes(footer[..8].try_into().unwrap());
        if len > MAX_DIRECTORY_LEN || len > size - FOOTER_LEN {
            return Err(invalid("invalid catalog directory length"));
        }
        let start = size - FOOTER_LEN - len;
        reader.seek(SeekFrom::Start(start))?;
        let directory: Directory =
            serde_json::from_reader((&mut reader).take(len)).map_err(invalid)?;
        if directory.providers.is_empty()
            || directory.providers.iter().any(|block| {
                block.len == 0
                    || block
                        .offset
                        .checked_add(block.len)
                        .is_none_or(|end| end > start)
            })
            || directory
                .providers
                .windows(2)
                .any(|pair| pair[0].id >= pair[1].id)
        {
            return Err(invalid("invalid catalog provider directory"));
        }
        Ok(Self {
            reader: Mutex::new(Box::new(reader)),
            directory,
        })
    }

    pub(super) fn expires(&self) -> u64 {
        self.directory.expires
    }

    pub(super) fn lookup(&self, keys: &[String], model: &str) -> io::Result<Option<ModelEntry>> {
        let mut reader = self.reader.lock().unwrap_or_else(|e| e.into_inner());
        let slug = model_slug(model);
        for key in keys {
            for name in [Some(model), (slug != model).then_some(slug.as_str())]
                .into_iter()
                .flatten()
            {
                for block in &self.directory.providers {
                    if block.id != *key && block.api.as_ref() != Some(key) {
                        continue;
                    }
                    reader.seek(SeekFrom::Start(block.offset))?;
                    let decoder = zstd::Decoder::new((&mut *reader).take(block.len))?;
                    let mut json = serde_json::Deserializer::from_reader(BufReader::new(decoder));
                    let entry = RequestedModel(name)
                        .deserialize(&mut json)
                        .map_err(invalid)?;
                    json.end().map_err(invalid)?;
                    if entry.is_some() {
                        return Ok(entry);
                    }
                }
            }
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests;

struct RequestedModel<'a>(&'a str);

impl<'de> DeserializeSeed<'de> for RequestedModel<'_> {
    type Value = Option<ModelEntry>;

    fn deserialize<D: serde::Deserializer<'de>>(
        self,
        deserializer: D,
    ) -> Result<Self::Value, D::Error> {
        deserializer.deserialize_map(self)
    }
}

impl<'de> Visitor<'de> for RequestedModel<'_> {
    type Value = Option<ModelEntry>;

    fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
        formatter.write_str("provider model entries")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let mut entry = None;
        while let Some(key) = map.next_key::<String>()? {
            if key == self.0 {
                entry = Some(map.next_value()?);
            } else {
                map.next_value::<IgnoredAny>()?;
            }
        }
        Ok(entry)
    }
}

/// Repack the upstream JSON one provider at a time. The download and the
/// complete parsed catalog never coexist in memory.
pub(super) fn write(
    reader: impl Read,
    mut writer: impl Write + Seek,
    expires: u64,
) -> io::Result<()> {
    let mut json = serde_json::Deserializer::from_reader(reader);
    let providers = WriteProviders(&mut writer)
        .deserialize(&mut json)
        .map_err(invalid)?;
    json.end().map_err(invalid)?;
    if providers.is_empty() {
        return Err(invalid("catalog contains no model metadata"));
    }
    let directory = Directory { expires, providers };
    let start = writer.stream_position()?;
    serde_json::to_writer(&mut writer, &directory).map_err(invalid)?;
    let len = writer.stream_position()? - start;
    if len > MAX_DIRECTORY_LEN {
        return Err(invalid("catalog directory is too large"));
    }
    writer.write_all(&len.to_le_bytes())?;
    writer.write_all(MAGIC)?;
    writer.flush()
}

struct WriteProviders<'a, W>(&'a mut W);

impl<'de, W: Write + Seek> DeserializeSeed<'de> for WriteProviders<'_, W> {
    type Value = Vec<ProviderBlock>;

    fn deserialize<D: serde::Deserializer<'de>>(
        self,
        deserializer: D,
    ) -> Result<Self::Value, D::Error> {
        deserializer.deserialize_map(self)
    }
}

impl<'de, W: Write + Seek> Visitor<'de> for WriteProviders<'_, W> {
    type Value = Vec<ProviderBlock>;

    fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
        formatter.write_str("models.dev providers")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        use serde::de::Error;

        let mut blocks = Vec::new();
        while let Some(id) = map.next_key::<String>()? {
            let mut provider = map.next_value::<Provider>()?;
            let api = provider.api.take().as_deref().and_then(api_key);
            let compiled = compile_provider(&id, provider);
            if compiled.models.is_empty() {
                continue;
            }
            let offset = self.0.stream_position().map_err(A::Error::custom)?;
            let mut encoder = zstd::Encoder::new(&mut *self.0, 3).map_err(A::Error::custom)?;
            encoder.include_checksum(true).map_err(A::Error::custom)?;
            serde_json::to_writer(&mut encoder, &compiled).map_err(A::Error::custom)?;
            encoder.finish().map_err(A::Error::custom)?;
            let len = self.0.stream_position().map_err(A::Error::custom)? - offset;
            blocks.push(ProviderBlock {
                id,
                api,
                offset,
                len,
            });
        }
        blocks.sort_by(|a, b| a.id.cmp(&b.id));
        if blocks.windows(2).any(|pair| pair[0].id == pair[1].id) {
            return Err(A::Error::custom("duplicate catalog provider"));
        }
        Ok(blocks)
    }
}
