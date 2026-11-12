/* -------------------------------------------------------------------------- */
/*                             memory/vectors.rs                              */
/* -------------------------------------------------------------------------- */

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::PathBuf;
use std::sync::{Mutex, PoisonError};

use anyhow::{Context, Result};
use rig_core::DynModel;
use rig_core::embeddings::{Embedding, EmbeddingsBuilder};
use rig_core::operation;
use rig_core::vector_store::VectorStoreIndex;
use rig_core::vector_store::in_memory_store::{InMemoryVectorIndex, InMemoryVectorStore};
use rig_core::vector_store::request::VectorSearchRequest;
use serde::{Deserialize, Serialize};

/// Each memory's embedding, by its text.
type Embedded = HashMap<String, Vec<f32>>;

/// Memories by meaning: rig's in-memory vector store over their embeddings,
/// which are kept in a file beside them so each is embedded once, when
/// first searched.
pub struct Vectors {
    reference: String,
    model: DynModel<operation::Embedding>,
    file: PathBuf,
    /// Held across each read, merge and write of the file.
    writing: Mutex<()>,
}

/// The file: embeddings are comparable only from the model that made them.
#[derive(Default, Serialize, Deserialize)]
struct Saved {
    model: String,
    embedded: Embedded,
}

impl Vectors {
    pub fn new(reference: &str, model: DynModel<operation::Embedding>, file: PathBuf) -> Self {
        Self {
            reference: reference.to_owned(),
            model,
            file,
            writing: Mutex::default(),
        }
    }

    /// Up to `limit` of `memories`, those closest in meaning to `query`, in
    /// the order they were saved. All of them, embedding nothing, when there
    /// are no more than `limit`.
    pub async fn closest<'a>(
        &self,
        memories: &'a [String],
        query: &str,
        limit: usize,
    ) -> Result<Vec<&'a String>> {
        if memories.len() <= limit {
            return Ok(memories.iter().collect());
        }
        let request = VectorSearchRequest::builder()
            .query(query)
            .samples(limit as u64)
            .build();
        let found = self
            .index(memories)
            .await?
            .top_n_ids(request)
            .await
            .context("cannot search memories")?;
        let mut chosen: Vec<usize> = found
            .into_iter()
            .filter_map(|found| found.id.parse().ok())
            .collect();
        chosen.sort_unstable();
        Ok(chosen.into_iter().filter_map(|i| memories.get(i)).collect())
    }

    /// A vector index of `memories`, each by its position, embedding those
    /// not yet embedded.
    async fn index(&self, memories: &[String]) -> Result<InMemoryVectorIndex<()>> {
        let saved = self.load();
        let missing: HashSet<&String> = memories
            .iter()
            .filter(|memory| !saved.contains_key(*memory))
            .collect();
        let embedded = match missing.is_empty() {
            true => saved,
            false => {
                let fresh = EmbeddingsBuilder::new(self.model.clone())
                    .documents(missing.into_iter().cloned())?
                    .build()
                    .await
                    .with_context(|| format!("cannot embed memories with `{}`", self.reference))?;
                self.save(memories, fresh)?
            }
        };
        let documents = memories.iter().enumerate().filter_map(|(i, memory)| {
            let vec = embedded.get(memory)?.iter().copied().map(f64::from).collect();
            let embedding = Embedding {
                document: memory.clone(),
                vec,
            };
            Some((i, (), vec![embedding]))
        });
        Ok(InMemoryVectorStore::from_documents_with_ids(documents).index(self.model.clone()))
    }

    /// The saved embeddings, if the configured model made them. The file is a
    /// cache: whatever cannot be read from it is embedded again.
    fn load(&self) -> Embedded {
        fs::read(&self.file)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Saved>(&bytes).ok())
            .filter(|saved| saved.model == self.reference)
            .map(|saved| saved.embedded)
            .unwrap_or_default()
    }

    /// Adds `fresh` to the saved embeddings, keeping only those of
    /// `memories`, and returns them.
    fn save(&self, memories: &[String], fresh: Vec<(String, Vec<Embedding>)>) -> Result<Embedded> {
        let _writing = self.writing.lock().unwrap_or_else(PoisonError::into_inner);
        let mut embedded = self.load();
        embedded.extend(fresh.into_iter().filter_map(|(memory, mut embeddings)| {
            let vec = embeddings.pop()?.vec.into_iter().map(|x| x as f32).collect();
            Some((memory, vec))
        }));
        let kept: HashSet<&String> = memories.iter().collect();
        embedded.retain(|memory, _| kept.contains(memory));
        let saved = Saved {
            model: self.reference.clone(),
            embedded,
        };
        super::replace(&self.file, &serde_json::to_string(&saved)?)
            .with_context(|| format!("cannot save embeddings {}", self.file.display()))?;
        Ok(saved.embedded)
    }
}
