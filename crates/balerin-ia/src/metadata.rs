//! The Internet Archive metadata read API: `GET /metadata/{identifier}`.
//!
//! Two quirks drive the shape of this module:
//!
//! 1. A missing item answers `[]` (an empty JSON array), not an error object,
//!    so we sniff the top-level type before deserialising.
//! 2. Numbers arrive as strings about half the time (`"size": "419170"`), and
//!    metadata values are string-or-array depending on cardinality.

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value};

use crate::client::IaClient;
use crate::error::{Error, Result};

pub const IA_BASE: &str = "https://archive.org";

/// The `format` value archive.org gives its auto-derived torrents.
pub const TORRENT_FORMAT: &str = "Archive BitTorrent";

/// One file within an item.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct IaFile {
    pub name: String,
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub format: Option<String>,
    #[serde(default, deserialize_with = "de_opt_u64")]
    pub size: Option<u64>,
    #[serde(default, deserialize_with = "de_opt_u64")]
    pub mtime: Option<u64>,
    #[serde(default)]
    pub md5: Option<String>,
    #[serde(default)]
    pub sha1: Option<String>,
    #[serde(default)]
    pub crc32: Option<String>,
    #[serde(default)]
    pub btih: Option<String>,
}

impl IaFile {
    pub fn is_torrent(&self) -> bool {
        self.format.as_deref() == Some(TORRENT_FORMAT) || self.name.ends_with(".torrent")
    }
}

/// The nested `metadata {}` object. Kept as a raw map because the schema is
/// per-collection freeform and every scalar may also appear as an array.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(transparent)]
pub struct Meta(pub Map<String, Value>);

impl Meta {
    /// First value for a key, as a string. Flattens single-element arrays.
    pub fn get_str(&self, key: &str) -> Option<&str> {
        match self.0.get(key)? {
            Value::String(s) => Some(s.as_str()),
            Value::Array(a) => a.first()?.as_str(),
            _ => None,
        }
    }

    /// All values for a key. Scalars come back as a one-element vec.
    pub fn get_all(&self, key: &str) -> Vec<&str> {
        match self.0.get(key) {
            Some(Value::String(s)) => vec![s.as_str()],
            Some(Value::Array(a)) => a.iter().filter_map(Value::as_str).collect(),
            _ => Vec::new(),
        }
    }

    pub fn title(&self) -> Option<&str> {
        self.get_str("title")
    }

    pub fn creator(&self) -> Option<&str> {
        self.get_str("creator")
    }

    pub fn mediatype(&self) -> Option<&str> {
        self.get_str("mediatype")
    }

    pub fn description(&self) -> Option<&str> {
        self.get_str("description")
    }

    pub fn publicdate(&self) -> Option<&str> {
        self.get_str("publicdate")
    }

    pub fn collections(&self) -> Vec<&str> {
        self.get_all("collection")
    }

    pub fn subjects(&self) -> Vec<&str> {
        self.get_all("subject")
    }
}

/// A parsed `/metadata/{identifier}` document.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ItemMetadata {
    /// Not part of the API response; filled in from the request.
    #[serde(default)]
    pub identifier: String,
    #[serde(default)]
    pub dir: Option<String>,
    #[serde(default)]
    pub server: Option<String>,
    #[serde(default)]
    pub d1: Option<String>,
    #[serde(default)]
    pub d2: Option<String>,
    #[serde(default)]
    pub workable_servers: Vec<String>,
    #[serde(default)]
    pub files: Vec<IaFile>,
    #[serde(default, deserialize_with = "de_opt_u64")]
    pub files_count: Option<u64>,
    #[serde(default, deserialize_with = "de_opt_u64")]
    pub item_size: Option<u64>,
    #[serde(default, deserialize_with = "de_opt_u64")]
    pub item_last_updated: Option<u64>,
    #[serde(default)]
    pub metadata: Meta,
}

impl ItemMetadata {
    /// Parse a metadata API body, mapping the empty-array response to
    /// [`Error::ItemNotFound`].
    pub fn parse(identifier: &str, body: &[u8]) -> Result<Self> {
        let value: Value = serde_json::from_slice(body).map_err(|source| Error::Json {
            context: format!("/metadata/{identifier}"),
            source,
        })?;
        match value {
            Value::Array(a) if a.is_empty() => Err(Error::ItemNotFound(identifier.to_string())),
            Value::Object(map) if map.is_empty() => {
                Err(Error::ItemNotFound(identifier.to_string()))
            }
            other => {
                let mut item: Self =
                    serde_json::from_value(other).map_err(|source| Error::Json {
                        context: format!("/metadata/{identifier}"),
                        source,
                    })?;
                item.identifier = identifier.to_string();
                Ok(item)
            }
        }
    }

    /// The auto-derived `.torrent`, if archive.org has made one.
    pub fn torrent_file(&self) -> Option<&IaFile> {
        self.files
            .iter()
            .find(|f| f.format.as_deref() == Some(TORRENT_FORMAT))
            .or_else(|| {
                self.files
                    .iter()
                    .find(|f| f.name == format!("{}_archive.torrent", self.identifier))
            })
    }

    /// Canonical download URL for a file. archive.org redirects this to a data
    /// node, which is exactly what we want for a webseed fallback.
    pub fn download_url(&self, file: &IaFile) -> String {
        format!(
            "{IA_BASE}/download/{}/{}",
            urlencoding::encode(&self.identifier),
            encode_path(&file.name)
        )
    }

    /// Direct data-node URL, skipping the redirect. `None` if the metadata
    /// response did not name a server and directory.
    pub fn node_url(&self, file: &IaFile) -> Option<String> {
        let server = self.server.as_deref().or(self.d1.as_deref())?;
        let dir = self.dir.as_deref()?;
        Some(format!("https://{server}{dir}/{}", encode_path(&file.name)))
    }

    pub fn torrent_url(&self) -> Option<String> {
        self.torrent_file().map(|f| self.download_url(f))
    }

    /// Data-node URLs suitable for use as BEP 19 webseed roots, in the order
    /// we would prefer to try them.
    pub fn webseed_roots(&self) -> Vec<String> {
        let mut roots = Vec::new();
        if let Some(dir) = self.dir.as_deref() {
            // The webseed root is the parent of the item directory, since
            // torrent paths are prefixed with the identifier.
            let parent = dir.rsplit_once('/').map(|(head, _)| head).unwrap_or("");
            for server in self.data_nodes() {
                roots.push(format!("https://{server}{parent}/"));
            }
        }
        roots.push(format!("{IA_BASE}/download/"));
        roots
    }

    /// Every data node archive.org mentioned, de-duplicated, preferred first.
    pub fn data_nodes(&self) -> Vec<&str> {
        let mut nodes: Vec<&str> = Vec::new();
        let candidates = self
            .server
            .as_deref()
            .into_iter()
            .chain(self.d1.as_deref())
            .chain(self.d2.as_deref())
            .chain(self.workable_servers.iter().map(String::as_str));
        for node in candidates {
            if !node.is_empty() && !nodes.contains(&node) {
                nodes.push(node);
            }
        }
        nodes
    }

    /// Sum of file sizes, for items where `item_size` is absent.
    pub fn total_size(&self) -> u64 {
        self.item_size
            .unwrap_or_else(|| self.files.iter().filter_map(|f| f.size).sum())
    }
}

/// Fetch and parse `/metadata/{identifier}`.
pub async fn fetch(client: &IaClient, identifier: &str) -> Result<ItemMetadata> {
    let url = format!("{IA_BASE}/metadata/{}", urlencoding::encode(identifier));
    let body = client.get(&url).await?;
    ItemMetadata::parse(identifier, &body)
}

/// Percent-encode a path, leaving separators alone.
fn encode_path(path: &str) -> String {
    path.split('/')
        .map(|seg| urlencoding::encode(seg).into_owned())
        .collect::<Vec<_>>()
        .join("/")
}

/// Accept `"419170"`, `419170` or `null` for the same field, because the
/// metadata API cheerfully mixes all three.
fn de_opt_u64<'de, D: Deserializer<'de>>(de: D) -> std::result::Result<Option<u64>, D::Error> {
    let value = Option::<Value>::deserialize(de)?;
    Ok(match value {
        Some(Value::Number(n)) => n.as_u64(),
        Some(Value::String(s)) => s.trim().parse().ok(),
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &[u8] = br#"{
        "created": 1616004182,
        "d1": "ia600308.us.archive.org",
        "d2": "ia800308.us.archive.org",
        "dir": "/21/items/xfetch",
        "files": [
            {"name": "xfetch.pdf", "source": "original", "format": "Text PDF",
             "mtime": "1479169618", "size": "419170", "md5": "abc", "crc32": "def", "sha1": "123"},
            {"name": "xfetch_archive.torrent", "source": "metadata",
             "format": "Archive BitTorrent", "size": 4523, "btih": "deadbeef"}
        ],
        "files_count": 13,
        "item_last_updated": 1613804036,
        "item_size": 10682344,
        "metadata": {
            "identifier": "xfetch",
            "mediatype": "texts",
            "collection": ["opensource", "community"],
            "title": "X-Fetch",
            "subject": "caching"
        },
        "server": "ia800308.us.archive.org",
        "workable_servers": ["ia800308.us.archive.org", "ia600308.us.archive.org"]
    }"#;

    fn sample() -> ItemMetadata {
        ItemMetadata::parse("xfetch", SAMPLE).expect("sample parses")
    }

    #[test]
    fn parses_string_and_numeric_sizes() {
        let item = sample();
        assert_eq!(item.files[0].size, Some(419_170));
        assert_eq!(item.files[0].mtime, Some(1_479_169_618));
        assert_eq!(item.files[1].size, Some(4523));
        assert_eq!(item.item_size, Some(10_682_344));
    }

    #[test]
    fn flattens_scalar_and_array_metadata() {
        let item = sample();
        assert_eq!(item.metadata.title(), Some("X-Fetch"));
        assert_eq!(item.metadata.mediatype(), Some("texts"));
        assert_eq!(item.metadata.collections(), vec!["opensource", "community"]);
        assert_eq!(item.metadata.subjects(), vec!["caching"]);
        assert!(item.metadata.creator().is_none());
    }

    #[test]
    fn missing_item_is_not_found_not_a_parse_error() {
        let err = ItemMetadata::parse("nope", b"[]").unwrap_err();
        assert!(matches!(err, Error::ItemNotFound(id) if id == "nope"));
    }

    #[test]
    fn finds_the_derived_torrent() {
        let item = sample();
        let torrent = item.torrent_file().expect("torrent present");
        assert_eq!(torrent.name, "xfetch_archive.torrent");
        assert_eq!(
            item.torrent_url().unwrap(),
            "https://archive.org/download/xfetch/xfetch_archive.torrent"
        );
    }

    #[test]
    fn builds_node_and_webseed_urls() {
        let item = sample();
        let pdf = &item.files[0];
        assert_eq!(
            item.node_url(pdf).unwrap(),
            "https://ia800308.us.archive.org/21/items/xfetch/xfetch.pdf"
        );
        let roots = item.webseed_roots();
        assert_eq!(roots[0], "https://ia800308.us.archive.org/21/items/");
        assert_eq!(roots.last().unwrap(), "https://archive.org/download/");
        // server, d1, d2 collapse to two distinct nodes plus the redirect root.
        assert_eq!(roots.len(), 3);
    }

    #[test]
    fn encodes_awkward_filenames() {
        let item = ItemMetadata {
            identifier: "some item".into(),
            ..Default::default()
        };
        let file = IaFile {
            name: "sub dir/track 01 & 02.mp3".into(),
            ..Default::default()
        };
        assert_eq!(
            item.download_url(&file),
            "https://archive.org/download/some%20item/sub%20dir/track%2001%20%26%2002.mp3"
        );
    }
}
