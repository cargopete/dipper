//! A local full-text index over harvested Internet Archive metadata.
//!
//! Searching archive.org over the network is slow and rate limited. Harvest
//! once into tantivy, then query locally in single-digit milliseconds.

use std::path::Path;

use tantivy::collector::{Count, TopDocs};
use tantivy::directory::MmapDirectory;
use tantivy::query::QueryParser;
use tantivy::schema::{FAST, Field, INDEXED, STORED, STRING, Schema, TEXT, TantivyDocument, Value};
use tantivy::{Index, IndexReader, IndexWriter, ReloadPolicy, Term, doc};

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("index error: {0}")]
    Tantivy(#[from] tantivy::TantivyError),

    #[error("could not open index directory: {0}")]
    Directory(#[from] tantivy::directory::error::OpenDirectoryError),

    #[error("bad query {query:?}: {source}")]
    Query {
        query: String,
        #[source]
        source: tantivy::query::QueryParserError,
    },
}

/// One indexed archive.org item.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Record {
    pub identifier: String,
    pub title: Option<String>,
    pub creator: Option<String>,
    pub description: Option<String>,
    pub mediatype: Option<String>,
    pub publicdate: Option<String>,
    pub subjects: Vec<String>,
    pub collections: Vec<String>,
    pub downloads: u64,
    pub item_size: u64,
    pub has_torrent: bool,
}

/// A search result: the stored record plus its BM25 score.
#[derive(Debug, Clone)]
pub struct Hit {
    pub record: Record,
    pub score: f32,
}

#[derive(Debug, Clone, Copy)]
struct Fields {
    identifier: Field,
    title: Field,
    creator: Field,
    description: Field,
    mediatype: Field,
    publicdate: Field,
    subject: Field,
    collection: Field,
    downloads: Field,
    item_size: Field,
    has_torrent: Field,
}

impl Fields {
    fn build() -> (Schema, Self) {
        let mut builder = Schema::builder();
        let fields = Self {
            // Exact-match key, so STRING rather than TEXT.
            identifier: builder.add_text_field("identifier", STRING | STORED),
            title: builder.add_text_field("title", TEXT | STORED),
            creator: builder.add_text_field("creator", TEXT | STORED),
            description: builder.add_text_field("description", TEXT | STORED),
            mediatype: builder.add_text_field("mediatype", STRING | STORED | FAST),
            publicdate: builder.add_text_field("publicdate", STRING | STORED),
            subject: builder.add_text_field("subject", TEXT | STORED),
            collection: builder.add_text_field("collection", TEXT | STORED),
            downloads: builder.add_u64_field("downloads", INDEXED | STORED | FAST),
            item_size: builder.add_u64_field("item_size", INDEXED | STORED | FAST),
            // Indexed as a keyword so `has_torrent:true` filters work.
            has_torrent: builder.add_text_field("has_torrent", STRING | STORED),
        };
        (builder.build(), fields)
    }
}

/// The local catalogue of harvested items.
pub struct Catalogue {
    index: Index,
    fields: Fields,
    reader: IndexReader,
}

impl Catalogue {
    /// Open (or create) an index in `dir`.
    pub fn open(dir: impl AsRef<Path>) -> Result<Self> {
        let dir = dir.as_ref();
        std::fs::create_dir_all(dir).map_err(tantivy::TantivyError::from)?;
        let (schema, fields) = Fields::build();
        let index = Index::open_or_create(MmapDirectory::open(dir)?, schema)?;
        Self::from_index(index, fields)
    }

    /// An index that lives only for this process. Handy for tests and for
    /// one-shot `--remote` searches.
    pub fn in_memory() -> Result<Self> {
        let (schema, fields) = Fields::build();
        Self::from_index(Index::create_in_ram(schema), fields)
    }

    fn from_index(index: Index, fields: Fields) -> Result<Self> {
        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::OnCommitWithDelay)
            .try_into()?;
        Ok(Self {
            index,
            fields,
            reader,
        })
    }

    /// A writer with a 50 MB heap, which is plenty for metadata documents.
    pub fn writer(&self) -> Result<Harvest> {
        Ok(Harvest {
            writer: self.index.writer(50_000_000)?,
            fields: self.fields,
        })
    }

    /// Query the catalogue. Bare terms search title, creator, description,
    /// subject and collection; field syntax (`mediatype:audio`,
    /// `has_torrent:true`) works as you would expect.
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<Hit>> {
        self.reader.reload()?;
        let searcher = self.reader.searcher();
        let parsed = self
            .parser()
            .parse_query(query)
            .map_err(|source| Error::Query {
                query: query.to_string(),
                source,
            })?;
        let top = searcher.search(&parsed, &TopDocs::with_limit(limit.max(1)).order_by_score())?;
        top.into_iter()
            .map(|(score, address)| {
                let doc: TantivyDocument = searcher.doc(address)?;
                Ok(Hit {
                    record: self.to_record(&doc),
                    score,
                })
            })
            .collect()
    }

    /// How many documents match a query.
    pub fn count(&self, query: &str) -> Result<usize> {
        self.reader.reload()?;
        let searcher = self.reader.searcher();
        let parsed = self
            .parser()
            .parse_query(query)
            .map_err(|source| Error::Query {
                query: query.to_string(),
                source,
            })?;
        Ok(searcher.search(&parsed, &Count)?)
    }

    /// Total documents in the catalogue.
    pub fn len(&self) -> Result<usize> {
        self.reader.reload()?;
        Ok(self.reader.searcher().num_docs() as usize)
    }

    pub fn is_empty(&self) -> Result<bool> {
        Ok(self.len()? == 0)
    }

    /// Look an item up by its exact identifier.
    pub fn get(&self, identifier: &str) -> Result<Option<Record>> {
        self.reader.reload()?;
        let searcher = self.reader.searcher();
        let term = Term::from_field_text(self.fields.identifier, identifier);
        let query = tantivy::query::TermQuery::new(term, tantivy::schema::IndexRecordOption::Basic);
        let top = searcher.search(&query, &TopDocs::with_limit(1).order_by_score())?;
        match top.first() {
            Some((_, address)) => {
                let doc: TantivyDocument = searcher.doc(*address)?;
                Ok(Some(self.to_record(&doc)))
            }
            None => Ok(None),
        }
    }

    fn parser(&self) -> QueryParser {
        let f = self.fields;
        let mut parser = QueryParser::for_index(
            &self.index,
            vec![f.title, f.creator, f.description, f.subject, f.collection],
        );
        // Titles are the most useful signal in archive.org metadata; the
        // description field is often a wall of boilerplate.
        parser.set_field_boost(f.title, 3.0);
        parser.set_field_boost(f.creator, 2.0);
        parser.set_field_boost(f.description, 0.5);
        parser
    }

    fn to_record(&self, doc: &TantivyDocument) -> Record {
        let f = self.fields;
        let text = |field: Field| {
            doc.get_first(field)
                .and_then(|v| v.as_str())
                .map(str::to_string)
        };
        let all = |field: Field| {
            doc.get_all(field)
                .filter_map(|v| v.as_str())
                .map(str::to_string)
                .collect()
        };
        let number = |field: Field| doc.get_first(field).and_then(|v| v.as_u64()).unwrap_or(0);

        Record {
            identifier: text(f.identifier).unwrap_or_default(),
            title: text(f.title),
            creator: text(f.creator),
            description: text(f.description),
            mediatype: text(f.mediatype),
            publicdate: text(f.publicdate),
            subjects: all(f.subject),
            collections: all(f.collection),
            downloads: number(f.downloads),
            item_size: number(f.item_size),
            has_torrent: text(f.has_torrent).as_deref() == Some("true"),
        }
    }
}

/// A batch of writes. Nothing is visible to searchers until [`Harvest::commit`].
pub struct Harvest {
    writer: IndexWriter,
    fields: Fields,
}

impl Harvest {
    /// Insert a record, replacing any existing one with the same identifier.
    pub fn upsert(&self, record: &Record) -> Result<()> {
        let f = self.fields;
        self.writer
            .delete_term(Term::from_field_text(f.identifier, &record.identifier));

        let mut doc = doc!(
            f.identifier => record.identifier.as_str(),
            f.downloads => record.downloads,
            f.item_size => record.item_size,
            f.has_torrent => if record.has_torrent { "true" } else { "false" },
        );
        let mut add = |field: Field, value: &Option<String>| {
            if let Some(value) = value {
                doc.add_text(field, value);
            }
        };
        add(f.title, &record.title);
        add(f.creator, &record.creator);
        add(f.description, &record.description);
        add(f.mediatype, &record.mediatype);
        add(f.publicdate, &record.publicdate);
        for subject in &record.subjects {
            doc.add_text(f.subject, subject);
        }
        for collection in &record.collections {
            doc.add_text(f.collection, collection);
        }

        self.writer.add_document(doc)?;
        Ok(())
    }

    /// Flush to disk and make the batch searchable.
    pub fn commit(&mut self) -> Result<()> {
        self.writer.commit()?;
        Ok(())
    }

    /// Throw the whole catalogue away.
    pub fn clear(&mut self) -> Result<()> {
        self.writer.delete_all_documents()?;
        self.commit()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(identifier: &str, title: &str, mediatype: &str, torrent: bool) -> Record {
        Record {
            identifier: identifier.into(),
            title: Some(title.into()),
            creator: Some("Some Body".into()),
            description: Some("a description of the thing".into()),
            mediatype: Some(mediatype.into()),
            publicdate: Some("2009-03-05T00:00:00Z".into()),
            subjects: vec!["birds".into(), "rivers".into()],
            collections: vec!["opensource".into()],
            downloads: 42,
            item_size: 1234,
            has_torrent: torrent,
        }
    }

    fn catalogue_with(records: &[Record]) -> Catalogue {
        let catalogue = Catalogue::in_memory().unwrap();
        let mut harvest = catalogue.writer().unwrap();
        for record in records {
            harvest.upsert(record).unwrap();
        }
        harvest.commit().unwrap();
        catalogue
    }

    #[test]
    fn round_trips_a_record() {
        let original = record("balerin-item", "The White-Throated Balerin", "texts", true);
        let catalogue = catalogue_with(std::slice::from_ref(&original));
        let stored = catalogue.get("balerin-item").unwrap().expect("stored");
        assert_eq!(stored, original);
    }

    #[test]
    fn finds_items_by_free_text() {
        let catalogue = catalogue_with(&[
            record("a", "The White-Throated Balerin", "texts", true),
            record("b", "Kingfishers of Britain", "texts", false),
        ]);
        let hits = catalogue.search("balerin", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].record.identifier, "a");
        assert!(hits[0].score > 0.0);
    }

    #[test]
    fn field_queries_filter() {
        let catalogue = catalogue_with(&[
            record("a", "Birdsong recordings", "audio", true),
            record("b", "Birdsong monograph", "texts", false),
        ]);
        assert_eq!(
            catalogue
                .search("birdsong AND mediatype:audio", 10)
                .unwrap()
                .len(),
            1
        );
        assert_eq!(catalogue.count("has_torrent:true").unwrap(), 1);
        assert_eq!(catalogue.count("subject:birds").unwrap(), 2);
    }

    #[test]
    fn upsert_replaces_rather_than_duplicates() {
        let catalogue = catalogue_with(&[record("a", "First title", "texts", false)]);
        let mut harvest = catalogue.writer().unwrap();
        harvest
            .upsert(&record("a", "Second title", "texts", true))
            .unwrap();
        harvest.commit().unwrap();

        assert_eq!(catalogue.len().unwrap(), 1);
        let stored = catalogue.get("a").unwrap().unwrap();
        assert_eq!(stored.title.as_deref(), Some("Second title"));
        assert!(stored.has_torrent);
    }

    #[test]
    fn survives_a_reopen() {
        let dir = tempfile::tempdir().unwrap();
        {
            let catalogue = Catalogue::open(dir.path()).unwrap();
            let mut harvest = catalogue.writer().unwrap();
            harvest
                .upsert(&record("a", "Persistent birds", "texts", true))
                .unwrap();
            harvest.commit().unwrap();
        }
        let reopened = Catalogue::open(dir.path()).unwrap();
        assert_eq!(reopened.len().unwrap(), 1);
        assert_eq!(reopened.search("persistent", 5).unwrap().len(), 1);
    }

    #[test]
    fn rejects_nonsense_queries_without_panicking() {
        let catalogue = catalogue_with(&[]);
        assert!(matches!(
            catalogue.search("mediatype:", 5),
            Err(Error::Query { .. })
        ));
    }

    #[test]
    fn clear_empties_the_catalogue() {
        let catalogue = catalogue_with(&[record("a", "Birds", "texts", true)]);
        let mut harvest = catalogue.writer().unwrap();
        harvest.clear().unwrap();
        assert!(catalogue.is_empty().unwrap());
    }
}
