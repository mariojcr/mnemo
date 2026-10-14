// SPDX-FileCopyrightText: 2026 Mario Campos <https://github.com/mariojcr>
// SPDX-License-Identifier: Apache-2.0

//! In-process embeddings: EmbeddingGemma 2 (ONNX, 768 dims) on ONNX Runtime, CPU only; mnemo
//! downloads nothing. fp32, not int8: int8's dynamic quantization is slower on short texts (53
//! tokens: 50 vs 113 ms) for the same vectors (cosine 0.9999). Measurements: docs/embeddings.md.

use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use tokenizers::{Tokenizer, TruncationParams};

/// Changing it re-embeds everything; it also selects EmbeddingGemma's prompt prefixes.
pub const MODEL_NAME: &str = "onnx-community/embeddinggemma-2-ONNX:fp32";
const REPO: &str = "onnx-community/embeddinggemma-2-ONNX";
const REVISION: &str = "daa72c51243991dfcaf9f9137d2c573d8f7790c0";
const FILES: [(&str, &str, u64); 3] = [
    (
        "onnx/model.onnx",
        "bc47de15f81208a5c99e5ab10f746d5e33b51ea228b7dc0bef9c133a94f1c1c3",
        421_421,
    ),
    (
        "onnx/model.onnx_data",
        "9fd452bfc6916e5a92f080f8dada35598ca4512b21c65897d4c5974d5f9630a2",
        1_084_170_240,
    ),
    (
        "tokenizer.json",
        "4d777ef5bdc1aa36227abdfb77c3e49e7b9c892d16e1b6bda41c393504828be4",
        32_170_510,
    ),
];
/// The graph also takes image, video and audio inputs; text-only use feeds them empty.
const MEDIA_DIM: usize = 512;
/// Positions (rows × length) per call: memory grows with the square of the length, so a batch of
/// long texts must not take the process down.
const SLOTS: usize = 8192;

pub struct Onnx {
    session: Mutex<ort::session::Session>,
    tok: Tokenizer,
    max_tokens: usize,
}

static RUNTIME: OnceLock<Result<(), String>> = OnceLock::new();

impl Onnx {
    pub fn load(
        dir: &Path,
        dylib: &str,
        threads: usize,
        max_tokens: usize,
    ) -> Result<Onnx, String> {
        RUNTIME
            .get_or_init(|| {
                ort::init_from(dylib)
                    .map_err(|e| format!("ONNX Runtime ({dylib}): {e}"))
                    .map(|b| {
                        b.commit();
                    })
            })
            .clone()?;
        let mut tok = Tokenizer::from_file(dir.join("tokenizer.json"))
            .map_err(|e| format!("tokenizer: {e}"))?;
        tok.with_truncation(Some(TruncationParams {
            max_length: max_tokens.max(16),
            ..Default::default()
        }))
        .map_err(|e| format!("tokenizer: {e}"))?;
        tok.with_padding(None);
        let builder = ort::session::Session::builder().map_err(|e| format!("ONNX model: {e}"))?;
        let mut builder = builder
            .with_intra_threads(threads.max(1))
            .map_err(|e| format!("ONNX model: {e}"))?;
        let session = builder
            .commit_from_file(dir.join(FILES[0].0))
            .map_err(|e| format!("ONNX model: {e}"))?;
        Ok(Onnx {
            session: Mutex::new(session),
            tok,
            max_tokens: max_tokens.max(16),
        })
    }

    /// Blocking: call it from `spawn_blocking`.
    pub fn embed(&self, texts: &[String]) -> Result<(Vec<Vec<f32>>, u64), String> {
        if texts.is_empty() {
            return Ok((Vec::new(), 0));
        }
        // Clip by characters first: tokenizing a multi-megabyte text costs unbounded memory and
        // time (~4 characters per token; 6 leaves headroom).
        let clipped: Vec<String> = texts
            .iter()
            .map(|t| t.chars().take(self.max_tokens * 6).collect())
            .collect();
        let enc = self
            .tok
            .encode_batch(clipped, true)
            .map_err(|e| format!("tokenizer: {e}"))?;
        let ids: Vec<Vec<u32>> = enc.iter().map(|e| e.get_ids().to_vec()).collect();
        let tokens = ids.iter().map(|v| v.len() as u64).sum();
        let mut out: Vec<Vec<f32>> = vec![Vec::new(); texts.len()];
        for group in groups(&ids, SLOTS) {
            let batch: Vec<Vec<u32>> = group.iter().map(|&i| ids[i].clone()).collect();
            for (i, v) in group.into_iter().zip(self.run(&batch)?) {
                out[i] = v;
            }
        }
        Ok((out, tokens))
    }

    fn run(&self, batch: &[Vec<u32>]) -> Result<Vec<Vec<f32>>, String> {
        let (ids, mask, len) = pad(batch);
        let rows = batch.len();
        let tensor = |v: Vec<i64>| ort::value::Tensor::from_array(([rows, len], v));
        let empty =
            || ort::value::Tensor::<f32>::from_array(([0usize, MEDIA_DIM], Vec::<f32>::new()));
        let (ids, mask, img, vid, aud) =
            match (tensor(ids), tensor(mask), empty(), empty(), empty()) {
                (Ok(a), Ok(b), Ok(c), Ok(d), Ok(e)) => (a, b, c, d, e),
                _ => return Err("ONNX: could not prepare the inputs".into()),
            };
        let mut s = self.session.lock().unwrap_or_else(|p| p.into_inner());
        let out = s
            .run(ort::inputs![
                "input_ids" => ids,
                "attention_mask" => mask,
                "image_features" => img,
                "video_features" => vid,
                "audio_features" => aud,
            ])
            .map_err(|e| format!("ONNX: {e}"))?;
        let (shape, data) = out["sentence_embedding"]
            .try_extract_tensor::<f32>()
            .map_err(|e| format!("ONNX: {e}"))?;
        let dims = shape.get(1).copied().unwrap_or(0) as usize;
        if dims == 0 || data.len() != rows * dims {
            return Err(format!("ONNX: unexpected output {shape:?}"));
        }
        Ok(data.chunks(dims).map(normalize).collect())
    }
}

fn groups(ids: &[Vec<u32>], slots: usize) -> Vec<Vec<usize>> {
    let mut order: Vec<usize> = (0..ids.len()).collect();
    order.sort_by_key(|&i| ids[i].len());
    let mut out: Vec<Vec<usize>> = Vec::new();
    let mut cur: Vec<usize> = Vec::new();
    for i in order {
        let longest = ids[i].len().max(1);
        if !cur.is_empty() && (cur.len() + 1) * longest > slots {
            out.push(std::mem::take(&mut cur));
        }
        cur.push(i);
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

fn pad(ids: &[Vec<u32>]) -> (Vec<i64>, Vec<i64>, usize) {
    let len = ids.iter().map(|v| v.len()).max().unwrap_or(0).max(1);
    let mut out = vec![0i64; ids.len() * len];
    let mut mask = vec![0i64; ids.len() * len];
    for (r, v) in ids.iter().enumerate() {
        for (c, id) in v.iter().enumerate() {
            out[r * len + c] = *id as i64;
            mask[r * len + c] = 1;
        }
    }
    (out, mask, len)
}

fn normalize(v: &[f32]) -> Vec<f32> {
    let n = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if n > 0.0 {
        v.iter().map(|x| x / n).collect()
    } else {
        v.to_vec()
    }
}

pub async fn check_files(dir: &Path) -> Result<(), String> {
    for (name, sha, size) in FILES {
        let path = dir.join(name);
        if !verified(&path, sha, size).await {
            return Err(format!(
                "{} is missing or is not the one from {REPO}@{REVISION} (sha256 {sha}, {size} bytes)",
                path.display()
            ));
        }
    }
    Ok(())
}

/// A sidecar `.sha256` marker avoids rehashing 1 GB on every start (a read-only directory, such
/// as the image's, rehashes: a couple of seconds).
async fn verified(path: &Path, sha: &str, size: u64) -> bool {
    let Ok(meta) = tokio::fs::metadata(path).await else {
        return false;
    };
    if meta.len() != size {
        return false;
    }
    let mark = PathBuf::from(format!("{}.sha256", path.display()));
    if tokio::fs::read_to_string(&mark)
        .await
        .is_ok_and(|m| m.trim() == sha)
    {
        return true;
    }
    let p = path.to_path_buf();
    let digest = tokio::task::spawn_blocking(move || -> Option<String> {
        use std::io::Read;
        let mut f = std::fs::File::open(p).ok()?;
        let mut h = Sha256::new();
        let mut buf = vec![0u8; 1 << 20];
        loop {
            let n = f.read(&mut buf).ok()?;
            if n == 0 {
                break;
            }
            h.update(&buf[..n]);
        }
        Some(format!("{:x}", h.finalize()))
    })
    .await
    .ok()
    .flatten();
    let ok = digest.as_deref() == Some(sha);
    if ok {
        mark_verified(path, sha).await;
    }
    ok
}

async fn mark_verified(path: &Path, sha: &str) {
    let _ = tokio::fs::write(format!("{}.sha256", path.display()), sha).await;
}

pub struct Local {
    model: OnceLock<Arc<Onnx>>,
    state: Mutex<String>,
    /// One inference at a time; a caller timing out while waiting starts nothing, so abandoned
    /// work does not pile up behind the model.
    turn: Arc<tokio::sync::Semaphore>,
}

impl Local {
    pub fn start(dir: PathBuf, dylib: String, threads: usize, max_tokens: usize) -> Arc<Local> {
        let me = Arc::new(Local {
            model: OnceLock::new(),
            state: Mutex::new("loading the embedding model".into()),
            turn: Arc::new(tokio::sync::Semaphore::new(1)),
        });
        let m = me.clone();
        tokio::spawn(async move {
            loop {
                let step = async {
                    check_files(&dir).await?;
                    let (d, l) = (dir.clone(), dylib.clone());
                    tokio::task::spawn_blocking(move || Onnx::load(&d, &l, threads, max_tokens))
                        .await
                        .map_err(|e| e.to_string())?
                };
                match step.await {
                    Ok(o) => {
                        let _ = m.model.set(Arc::new(o));
                        log::info!(
                            "embeddings: {MODEL_NAME} ready ({} threads, up to {max_tokens} tokens)",
                            threads.max(1)
                        );
                        return;
                    }
                    Err(e) => {
                        log::warn!("embeddings: {e}; retrying in a minute");
                        *m.state.lock().unwrap_or_else(|p| p.into_inner()) = e;
                        tokio::time::sleep(Duration::from_secs(60)).await;
                    }
                }
            }
        });
        me
    }

    pub fn ready(&self) -> Result<(), String> {
        match self.model.get() {
            Some(_) => Ok(()),
            None => Err(self.state.lock().unwrap_or_else(|p| p.into_inner()).clone()),
        }
    }

    /// The permit travels with the inference: if the caller stops waiting, it stays held until the
    /// inference ends, so no other one starts.
    pub async fn embed(
        &self,
        texts: &[String],
        deadline: Duration,
    ) -> Result<Vec<Vec<f32>>, String> {
        let Some(m) = self.model.get().cloned() else {
            return Err(self.ready().err().unwrap_or_default());
        };
        let permit = tokio::time::timeout(deadline, self.turn.clone().acquire_owned())
            .await
            .map_err(|_| "embeddings: the model is busy".to_string())?
            .map_err(|e| e.to_string())?;
        let texts = texts.to_vec();
        // Time the inference, not the wait for a turn: that measures the model's cost.
        let started = std::time::Instant::now();
        let r = tokio::task::spawn_blocking(move || {
            let _turn = permit;
            m.embed(&texts)
        })
        .await
        .map_err(|e| format!("embeddings: {e}"))
        .and_then(|r| r);
        let ms = started.elapsed().as_millis() as u64;
        match r {
            Ok((v, tokens)) => {
                let usage = serde_json::json!({ "prompt_tokens": tokens });
                crate::meter::llm("embed", MODEL_NAME, Some(&usage), ms, true);
                Ok(v)
            }
            Err(e) => {
                crate::meter::llm("embed", MODEL_NAME, None, ms, false);
                Err(e)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn missing_model_files_are_reported_and_never_downloaded() {
        let dir = std::env::temp_dir().join(format!("mnemo-onnx-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let e = check_files(&dir).await.unwrap_err();
        assert!(e.contains(REVISION) && e.contains("model.onnx"), "{e}");
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_batch_is_padded_to_its_longest_text_with_a_matching_mask() {
        let (ids, mask, len) = pad(&[vec![5, 6, 7], vec![9]]);
        assert_eq!(len, 3);
        assert_eq!(ids, vec![5, 6, 7, 9, 0, 0]);
        assert_eq!(mask, vec![1, 1, 1, 1, 0, 0]);
    }

    #[test]
    fn long_texts_are_split_into_batches_within_the_slot_budget() {
        let ids: Vec<Vec<u32>> = [3000, 10, 3000, 20, 3000]
            .iter()
            .map(|&n| vec![1; n])
            .collect();
        let g = groups(&ids, 8192);
        for b in &g {
            let longest = b.iter().map(|&i| ids[i].len()).max().unwrap();
            assert!(b.len() * longest <= 8192 || b.len() == 1, "{b:?}");
        }
        let mut all: Vec<usize> = g.into_iter().flatten().collect();
        all.sort();
        assert_eq!(all, vec![0, 1, 2, 3, 4], "none is lost or repeated");
    }

    #[test]
    fn vectors_come_out_unit_length() {
        let v = normalize(&[3.0, 4.0]);
        assert!((v[0] - 0.6).abs() < 1e-6 && (v[1] - 0.8).abs() < 1e-6);
        assert_eq!(normalize(&[0.0, 0.0]), vec![0.0, 0.0]);
    }
}
