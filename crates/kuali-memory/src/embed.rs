//! Turning text into vectors so a question can find a passage that never used
//! its words.
//!
//! Lexical search answers "firewall" with passages containing "firewall". This
//! answers it with "el cortafuegos estaba bloqueando el puerto", which is the
//! same fact written by someone who was speaking Spanish at the time. That gap
//! is the whole reason this module exists.
//!
//! The model is `multilingual-e5-small`, quantized to int8 and run through ONNX
//! Runtime on the CPU. Three properties made it the choice: it is genuinely
//! multilingual, ONNX Runtime links statically so nothing extra has to be signed
//! or shipped beside the binary, and int8 keeps the download near 128 MB.
//!
//! # Lifecycle
//!
//! Loading costs about 130 MB of resident memory, so the model follows the same
//! rule as Whisper: it is loaded when there is work, and dropped when there is
//! not. Unlike Whisper it is never held for the length of a call — embedding a
//! finished meeting takes about a second, and embedding one question takes
//! milliseconds.

use std::path::{Path, PathBuf};

use ndarray::Array2;
use ort::session::{builder::GraphOptimizationLevel, Session};
use ort::value::Value;
use tokenizers::Tokenizer;

use crate::MemoryError;

/// Width of one vector. Fixed by the model; stored so a future model change is
/// detected instead of silently comparing vectors of different shapes.
pub const DIMENSIONS: usize = 384;

/// Identifier written beside every vector. Changing the model changes this, and
/// vectors produced by a different one are rebuilt rather than trusted.
pub const MODEL_ID: &str = "multilingual-e5-small-int8";

/// Texts embedded per forward pass.
///
/// Measured on Apple Silicon, throughput flattens at eight: larger batches cost
/// memory without going faster, and smaller ones pay the per-call overhead more
/// often.
const BATCH: usize = 8;

/// Tokens kept from one passage. The model itself accepts 512; a Kuali chunk is
/// bounded well below that, so this only guards against a pathological turn.
const MAX_TOKENS: usize = 512;

/// `e5` was trained with these markers and behaves noticeably worse without
/// them. A question and a passage are asymmetric: the model is told which is
/// which.
const QUERY_PREFIX: &str = "query: ";
const PASSAGE_PREFIX: &str = "passage: ";

/// Where the weights live, under the directory the user already chose for
/// Whisper so one setting governs every model Kuali downloads.
pub fn model_dir(models_dir: &Path) -> PathBuf {
    models_dir.join("embeddings")
}

pub fn model_path(models_dir: &Path) -> PathBuf {
    model_dir(models_dir).join("multilingual-e5-small-int8.onnx")
}

pub fn tokenizer_path(models_dir: &Path) -> PathBuf {
    model_dir(models_dir).join("multilingual-e5-small-tokenizer.json")
}

/// Whether both files needed to embed anything are present.
pub fn is_downloaded(models_dir: &Path) -> bool {
    model_path(models_dir).is_file() && tokenizer_path(models_dir).is_file()
}

/// A loaded embedding model. Dropping it releases the memory.
pub struct Embedder {
    session: Session,
    tokenizer: Tokenizer,
    /// Whether the exported graph asks for `token_type_ids`. The tensor is all
    /// zeros for this architecture, but some exports declare the input and
    /// refuse to run without it while others omit it entirely.
    wants_token_type_ids: bool,
}

impl Embedder {
    pub fn load(models_dir: &Path) -> Result<Self, MemoryError> {
        let model = model_path(models_dir);
        let tokenizer = tokenizer_path(models_dir);
        if !model.is_file() || !tokenizer.is_file() {
            return Err(MemoryError::EmbeddingModelMissing);
        }

        let tokenizer =
            Tokenizer::from_file(&tokenizer).map_err(|error| MemoryError::Embedding {
                message: format!("no pude leer el tokenizador: {error}"),
            })?;
        // Written as steps rather than a chain: `ort::Error` carries the builder
        // type as a parameter, so each stage fails with a different error type.
        let loading = |error: &dyn std::fmt::Display| MemoryError::Embedding {
            message: format!("no pude cargar el modelo de embeddings: {error}"),
        };
        let builder = Session::builder().map_err(|error| loading(&error))?;
        let mut builder = builder
            .with_optimization_level(GraphOptimizationLevel::Level3)
            .map_err(|error| loading(&error))?;
        let session = builder
            .commit_from_file(&model)
            .map_err(|error| loading(&error))?;

        let wants_token_type_ids = session
            .inputs()
            .iter()
            .any(|input| input.name() == "token_type_ids");

        Ok(Self {
            session,
            tokenizer,
            wants_token_type_ids,
        })
    }

    /// Embeds passages for storage in the index.
    pub fn embed_passages(&mut self, texts: &[String]) -> Result<Vec<Vec<f32>>, MemoryError> {
        self.embed(texts, PASSAGE_PREFIX)
    }

    /// Embeds one question for comparison against stored passages.
    pub fn embed_query(&mut self, text: &str) -> Result<Vec<f32>, MemoryError> {
        Ok(self
            .embed(std::slice::from_ref(&text.to_string()), QUERY_PREFIX)?
            .pop()
            .unwrap_or_else(|| vec![0.0; DIMENSIONS]))
    }

    fn embed(&mut self, texts: &[String], prefix: &str) -> Result<Vec<Vec<f32>>, MemoryError> {
        let mut vectors = Vec::with_capacity(texts.len());
        for group in texts.chunks(BATCH) {
            let prefixed: Vec<String> = group
                .iter()
                .map(|text| format!("{prefix}{}", text.trim()))
                .collect();
            vectors.extend(self.forward(&prefixed)?);
        }
        Ok(vectors)
    }

    fn forward(&mut self, texts: &[String]) -> Result<Vec<Vec<f32>>, MemoryError> {
        let encodings = self
            .tokenizer
            .encode_batch(texts.to_vec(), true)
            .map_err(|error| MemoryError::Embedding {
                message: format!("no pude tokenizar: {error}"),
            })?;

        let rows = encodings.len();
        let width = encodings
            .iter()
            .map(|encoding| encoding.len().min(MAX_TOKENS))
            .max()
            .unwrap_or(1)
            .max(1);

        let mut ids = Array2::<i64>::zeros((rows, width));
        let mut mask = Array2::<i64>::zeros((rows, width));
        for (row, encoding) in encodings.iter().enumerate() {
            let taken = encoding.len().min(width);
            for column in 0..taken {
                ids[[row, column]] = i64::from(encoding.get_ids()[column]);
                mask[[row, column]] = i64::from(encoding.get_attention_mask()[column]);
            }
        }

        let embedding_error = |error: ort::Error<_>| MemoryError::Embedding {
            message: format!("falló la inferencia de embeddings: {error}"),
        };

        let mut inputs = ort::inputs![
            "input_ids" => Value::from_array(ids).map_err(embedding_error)?,
            "attention_mask" => Value::from_array(mask.clone()).map_err(embedding_error)?
        ];
        if self.wants_token_type_ids {
            inputs.push((
                "token_type_ids".into(),
                Value::from_array(Array2::<i64>::zeros((rows, width)))
                    .map_err(embedding_error)?
                    .into(),
            ));
        }

        let outputs = self
            .session
            .run(inputs)
            .map_err(|error| MemoryError::Embedding {
                message: format!("falló la inferencia de embeddings: {error}"),
            })?;
        let (shape, data) =
            outputs[0]
                .try_extract_tensor::<f32>()
                .map_err(|error| MemoryError::Embedding {
                    message: format!("la salida del modelo no tenía la forma esperada: {error}"),
                })?;

        let sequence = shape[1] as usize;
        let dimensions = shape[2] as usize;
        if dimensions != DIMENSIONS {
            return Err(MemoryError::Embedding {
                message: format!(
                    "el modelo devolvió {dimensions} dimensiones, se esperaban {DIMENSIONS}"
                ),
            });
        }

        Ok((0..rows)
            .map(|row| pool(data, &mask, row, sequence, dimensions))
            .collect())
    }
}

/// Mean pooling over the tokens that are really there, then L2 normalization.
///
/// Padding is excluded through the attention mask, otherwise a short passage in
/// a batch with a long one would have its meaning diluted by the padding it
/// never contained. Normalizing afterwards makes the dot product a cosine, so
/// searching is one multiply-and-add per dimension.
fn pool(
    data: &[f32],
    mask: &Array2<i64>,
    row: usize,
    sequence: usize,
    dimensions: usize,
) -> Vec<f32> {
    let mut pooled = vec![0f32; dimensions];
    let mut counted = 0f32;
    for token in 0..sequence {
        if mask[[row, token]] == 0 {
            continue;
        }
        counted += 1.0;
        let base = (row * sequence + token) * dimensions;
        for (dimension, value) in pooled.iter_mut().enumerate() {
            *value += data[base + dimension];
        }
    }
    if counted > 0.0 {
        for value in pooled.iter_mut() {
            *value /= counted;
        }
    }
    normalize(&mut pooled);
    pooled
}

fn normalize(vector: &mut [f32]) {
    let length = vector.iter().map(|value| value * value).sum::<f32>().sqrt();
    if length > 1e-12 {
        for value in vector.iter_mut() {
            *value /= length;
        }
    }
}

/// Similarity between two normalized vectors. Both come out of [`pool`], so the
/// dot product already is the cosine.
pub fn similarity(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

/// Packs a vector for storage. Little-endian `f32` keeps the file portable
/// between the platforms Kuali targets.
pub fn to_bytes(vector: &[f32]) -> Vec<u8> {
    vector
        .iter()
        .flat_map(|value| value.to_le_bytes())
        .collect()
}

/// Reads a stored vector back. A blob of the wrong width is refused rather than
/// reinterpreted, because a truncated vector would silently score as garbage.
pub fn from_bytes(bytes: &[u8]) -> Option<Vec<f32>> {
    if bytes.len() != DIMENSIONS * 4 {
        return None;
    }
    Some(
        bytes
            .chunks_exact(4)
            .map(|chunk| f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_vector_survives_the_trip_to_storage_and_back() {
        let vector: Vec<f32> = (0..DIMENSIONS).map(|i| i as f32 / 100.0).collect();
        let restored = from_bytes(&to_bytes(&vector)).expect("a full vector round-trips");
        assert_eq!(restored, vector);
    }

    #[test]
    fn a_vector_of_the_wrong_width_is_refused_rather_than_reinterpreted() {
        assert_eq!(from_bytes(&[0u8; 16]), None);
        assert_eq!(from_bytes(&[]), None);
    }

    #[test]
    fn normalizing_makes_the_dot_product_a_cosine() {
        let mut a = vec![3.0, 4.0];
        normalize(&mut a);
        assert!((a.iter().map(|v| v * v).sum::<f32>() - 1.0).abs() < 1e-6);
        assert!((similarity(&a, &a) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn a_zero_vector_normalizes_without_dividing_by_zero() {
        let mut zero = vec![0.0; 8];
        normalize(&mut zero);
        assert!(zero.iter().all(|value| value.is_finite()));
    }
}

/// Checks the real model rather than a stub.
///
/// Ignored by default because it needs the weights on disk, which most
/// contributors and CI will not have. Run it with the model downloaded:
///
/// ```text
/// KUALI_EMBED_MODELS_DIR=/path/to/models cargo test -p kuali-memory -- --ignored
/// ```
#[cfg(test)]
mod model_tests {
    use super::*;

    fn embedder() -> Option<Embedder> {
        let dir = std::env::var("KUALI_EMBED_MODELS_DIR").ok()?;
        Embedder::load(Path::new(&dir)).ok()
    }

    #[test]
    #[ignore = "needs the embedding model on disk"]
    fn a_question_finds_a_passage_that_never_used_its_words() {
        let Some(mut embedder) = embedder() else {
            panic!("set KUALI_EMBED_MODELS_DIR to the folder holding the model");
        };

        let question = embedder
            .embed_query("¿qué problema tuvo Sebas con el firewall?")
            .unwrap();
        let passages = embedder
            .embed_passages(&[
                "Sebas comentó que el cortafuegos estaba bloqueando el puerto 8080.".into(),
                "Hablamos del presupuesto de marketing para el trimestre.".into(),
            ])
            .unwrap();

        let about_firewall = similarity(&question, &passages[0]);
        let about_budget = similarity(&question, &passages[1]);

        // Neither passage contains the word "firewall". Only meaning connects
        // the question to the first one, which is the entire point.
        assert!(
            about_firewall > about_budget,
            "expected the firewall passage to win: {about_firewall} vs {about_budget}"
        );
    }

    #[test]
    #[ignore = "needs the embedding model on disk"]
    fn every_vector_is_normalized_and_the_expected_width() {
        let Some(mut embedder) = embedder() else {
            panic!("set KUALI_EMBED_MODELS_DIR to the folder holding the model");
        };

        for vector in embedder
            .embed_passages(&["hola qué tal".into(), "otra cosa distinta".into()])
            .unwrap()
        {
            assert_eq!(vector.len(), DIMENSIONS);
            let length = vector.iter().map(|v| v * v).sum::<f32>().sqrt();
            assert!((length - 1.0).abs() < 1e-4, "vector was not normalized");
        }
    }
}
