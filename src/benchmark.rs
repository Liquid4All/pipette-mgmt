use std::collections::HashMap;
use std::path::Path;

use serde::{Deserialize, Serialize};
use strum::{AsRefStr, Display, EnumString};

use crate::types::BenchmarkId;

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Display, EnumString, AsRefStr,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum BenchmarkType {
    PrefillThroughput,
    DecodeThroughput,
    EndToEndLatency,
    MaxMemoryUsage,
    Eval,
    VlThroughput,
    VlMaxMemory,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "benchmark_type")]
pub enum BenchmarkDef {
    #[serde(rename = "prefill_throughput")]
    PrefillThroughput { parameter_prefill_tokens: i32 },
    #[serde(rename = "decode_throughput")]
    DecodeThroughput {
        parameter_prefill_tokens: i32,
        parameter_decode_tokens: i32,
    },
    #[serde(rename = "end_to_end_latency")]
    EndToEndLatency {
        parameter_prefill_tokens: i32,
        parameter_decode_tokens: i32,
    },
    #[serde(rename = "max_memory_usage")]
    MaxMemoryUsage { parameter_prefill_tokens: i32 },
    #[serde(rename = "eval")]
    Eval {
        parameter_eval_id: String,
        parameter_dataset_name: String,
        parameter_max_tokens: i32,
        #[serde(default)]
        parameter_mcq_choices: Option<Vec<String>>,
    },
    #[serde(rename = "vl_throughput")]
    VlThroughput {
        parameter_image_width: i32,
        parameter_image_height: i32,
        parameter_text_tokens: i32,
        parameter_decode_tokens: i32,
        /// Images in the prompt (>1 = multi-frame/video); defaults to 1.
        #[serde(default = "default_num_images")]
        parameter_num_images: i32,
    },
    #[serde(rename = "vl_max_memory")]
    VlMaxMemory {
        parameter_image_width: i32,
        parameter_image_height: i32,
        #[serde(default)]
        parameter_text_tokens: i32,
        #[serde(default = "default_num_images")]
        parameter_num_images: i32,
    },
}

fn default_num_images() -> i32 {
    1
}

#[derive(Debug, Clone, Serialize)]
pub struct Benchmark {
    pub benchmark_id: BenchmarkId,
    #[serde(flatten)]
    pub def: BenchmarkDef,
}

impl Benchmark {
    pub fn benchmark_type(&self) -> BenchmarkType {
        match &self.def {
            BenchmarkDef::PrefillThroughput { .. } => BenchmarkType::PrefillThroughput,
            BenchmarkDef::DecodeThroughput { .. } => BenchmarkType::DecodeThroughput,
            BenchmarkDef::EndToEndLatency { .. } => BenchmarkType::EndToEndLatency,
            BenchmarkDef::MaxMemoryUsage { .. } => BenchmarkType::MaxMemoryUsage,
            BenchmarkDef::Eval { .. } => BenchmarkType::Eval,
            BenchmarkDef::VlThroughput { .. } => BenchmarkType::VlThroughput,
            BenchmarkDef::VlMaxMemory { .. } => BenchmarkType::VlMaxMemory,
        }
    }

    pub(crate) fn from_toml(benchmark_id: &str, content: &str) -> anyhow::Result<Self> {
        let def: BenchmarkDef = toml::from_str(content)
            .map_err(|e| anyhow::anyhow!("failed to parse benchmark {benchmark_id}: {e}"))?;
        Ok(Self {
            benchmark_id: BenchmarkId::try_new(benchmark_id)?,
            def,
        })
    }
}

pub fn load_catalog(benchmarks_dir: &Path) -> anyhow::Result<HashMap<BenchmarkId, Benchmark>> {
    if !benchmarks_dir.exists() {
        anyhow::bail!(
            "benchmarks directory does not exist: {}",
            benchmarks_dir.display()
        );
    }

    let entries: Vec<_> = std::fs::read_dir(benchmarks_dir)?
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .filter(|e| e.path().extension().is_some_and(|ext| ext == "toml"))
        .collect();

    if entries.is_empty() {
        anyhow::bail!("no benchmark files found in {}", benchmarks_dir.display());
    }

    let catalog = entries
        .into_iter()
        .map(|entry| {
            let path = entry.path();
            let benchmark_id = path
                .file_stem()
                .ok_or_else(|| anyhow::anyhow!("missing file stem for {}", path.display()))?
                .to_string_lossy()
                .to_string();
            let content = std::fs::read_to_string(&path)?;
            let benchmark = Benchmark::from_toml(&benchmark_id, &content)?;
            Ok((BenchmarkId::try_new(benchmark_id)?, benchmark))
        })
        .collect::<anyhow::Result<HashMap<_, _>>>()?;

    Ok(catalog)
}

#[cfg(test)]
mod tests {
    use crate::benchmark::*;
    use anyhow::Context;
    use rstest::rstest;

    #[test]
    fn test_parse_prefill_throughput() -> anyhow::Result<()> {
        let toml = r#"
benchmark_type = "prefill_throughput"
parameter_prefill_tokens = 256
"#;
        let def: BenchmarkDef = toml::from_str(toml)?;
        assert!(matches!(def, BenchmarkDef::PrefillThroughput { .. }));
        Ok(())
    }

    #[test]
    fn test_parse_decode_throughput() -> anyhow::Result<()> {
        let toml = r#"
benchmark_type = "decode_throughput"
parameter_prefill_tokens = 512
parameter_decode_tokens = 100
"#;
        let def: BenchmarkDef = toml::from_str(toml)?;
        assert!(matches!(def, BenchmarkDef::DecodeThroughput { .. }));
        Ok(())
    }

    #[test]
    fn test_parse_end_to_end_latency() -> anyhow::Result<()> {
        let toml = r#"
benchmark_type = "end_to_end_latency"
parameter_prefill_tokens = 256
parameter_decode_tokens = 256
"#;
        let def: BenchmarkDef = toml::from_str(toml)?;
        assert!(matches!(def, BenchmarkDef::EndToEndLatency { .. }));
        Ok(())
    }

    #[test]
    fn test_parse_max_memory_usage() -> anyhow::Result<()> {
        let toml = r#"
benchmark_type = "max_memory_usage"
parameter_prefill_tokens = 256
"#;
        let def: BenchmarkDef = toml::from_str(toml)?;
        assert!(matches!(def, BenchmarkDef::MaxMemoryUsage { .. }));
        Ok(())
    }

    #[test]
    fn test_parse_eval() -> anyhow::Result<()> {
        let toml = r#"
benchmark_type = "eval"
parameter_eval_id = "mmlu_pro"
parameter_dataset_name = "edge_2026.03.2"
parameter_max_tokens = 1024
parameter_mcq_choices = ["A", "B", "C", "D"]
"#;
        let def: BenchmarkDef = toml::from_str(toml)?;
        match def {
            BenchmarkDef::Eval {
                parameter_mcq_choices,
                ..
            } => {
                let choices = parameter_mcq_choices.context("expected mcq_choices")?;
                assert_eq!(choices, vec!["A", "B", "C", "D"]);
            }
            _ => panic!("expected Eval"),
        }
        Ok(())
    }

    #[test]
    fn test_parse_eval_without_mcq() -> anyhow::Result<()> {
        let toml = r#"
benchmark_type = "eval"
parameter_eval_id = "ifeval"
parameter_dataset_name = "edge_2026.03.2"
parameter_max_tokens = 4096
"#;
        let def: BenchmarkDef = toml::from_str(toml)?;
        match def {
            BenchmarkDef::Eval {
                parameter_mcq_choices,
                ..
            } => {
                assert!(parameter_mcq_choices.is_none());
            }
            _ => panic!("expected Eval"),
        }
        Ok(())
    }

    #[test]
    fn test_load_catalog() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        std::fs::write(
            dir.path().join("prefill_throughput_256.toml"),
            r#"benchmark_type = "prefill_throughput"
parameter_prefill_tokens = 256"#,
        )?;
        std::fs::write(
            dir.path().join("decode_throughput_512_100.toml"),
            r#"benchmark_type = "decode_throughput"
parameter_prefill_tokens = 512
parameter_decode_tokens = 100"#,
        )?;

        let catalog = load_catalog(dir.path())?;
        assert_eq!(catalog.len(), 2);
        assert!(catalog.contains_key(&BenchmarkId::try_new("prefill_throughput_256")?));
        assert!(catalog.contains_key(&BenchmarkId::try_new("decode_throughput_512_100")?));
        Ok(())
    }

    #[test]
    fn test_load_catalog_missing_dir() {
        let result = load_catalog(Path::new("/nonexistent"));
        assert!(result.is_err());
    }

    #[test]
    fn test_benchmark_type() -> anyhow::Result<()> {
        let b = Benchmark {
            benchmark_id: BenchmarkId::try_new("test")?,
            def: BenchmarkDef::PrefillThroughput {
                parameter_prefill_tokens: 256,
            },
        };
        assert_eq!(b.benchmark_type(), BenchmarkType::PrefillThroughput);
        Ok(())
    }

    #[test]
    fn test_parse_vl_throughput() -> anyhow::Result<()> {
        let toml = r#"
benchmark_type = "vl_throughput"
parameter_image_width = 384
parameter_image_height = 512
parameter_text_tokens = 32
parameter_decode_tokens = 128
"#;
        let def: BenchmarkDef = toml::from_str(toml)?;
        match def {
            BenchmarkDef::VlThroughput {
                parameter_image_width,
                parameter_image_height,
                parameter_text_tokens,
                parameter_decode_tokens,
                parameter_num_images,
            } => {
                assert_eq!(parameter_image_width, 384);
                assert_eq!(parameter_image_height, 512);
                assert_eq!(parameter_text_tokens, 32);
                assert_eq!(parameter_decode_tokens, 128);
                assert_eq!(parameter_num_images, 1);
            }
            _ => panic!("expected VlThroughput"),
        }
        Ok(())
    }

    // `explicit` sets every field; `defaults` omits text/num_images to exercise
    // the serde defaults (0 text tokens, 1 image).
    #[rstest]
    #[case::explicit(
        r#"
benchmark_type = "vl_max_memory"
parameter_image_width = 512
parameter_image_height = 512
parameter_text_tokens = 1024
parameter_num_images = 5
"#,
        512,
        512,
        1024,
        5
    )]
    #[case::defaults(
        r#"
benchmark_type = "vl_max_memory"
parameter_image_width = 256
parameter_image_height = 256
"#,
        256,
        256,
        0,
        1
    )]
    fn test_parse_vl_max_memory(
        #[case] toml: &str,
        #[case] width: i32,
        #[case] height: i32,
        #[case] text_tokens: i32,
        #[case] num_images: i32,
    ) -> anyhow::Result<()> {
        let def: BenchmarkDef = toml::from_str(toml)?;
        match def {
            BenchmarkDef::VlMaxMemory {
                parameter_image_width,
                parameter_image_height,
                parameter_text_tokens,
                parameter_num_images,
            } => {
                assert_eq!(parameter_image_width, width);
                assert_eq!(parameter_image_height, height);
                assert_eq!(parameter_text_tokens, text_tokens);
                assert_eq!(parameter_num_images, num_images);
            }
            _ => panic!("expected VlMaxMemory"),
        }
        Ok(())
    }

    #[test]
    fn test_vl_max_memory_type() -> anyhow::Result<()> {
        let b = Benchmark {
            benchmark_id: BenchmarkId::try_new("vl_max_memory_512x512_t0_f1")?,
            def: BenchmarkDef::VlMaxMemory {
                parameter_image_width: 512,
                parameter_image_height: 512,
                parameter_text_tokens: 0,
                parameter_num_images: 1,
            },
        };
        assert_eq!(b.benchmark_type(), BenchmarkType::VlMaxMemory);
        Ok(())
    }

    #[test]
    fn test_vl_throughput_type() -> anyhow::Result<()> {
        let b = Benchmark {
            benchmark_id: BenchmarkId::try_new("vl_throughput_384x512_32_128")?,
            def: BenchmarkDef::VlThroughput {
                parameter_image_width: 384,
                parameter_image_height: 512,
                parameter_text_tokens: 32,
                parameter_decode_tokens: 128,
                parameter_num_images: 1,
            },
        };
        assert_eq!(b.benchmark_type(), BenchmarkType::VlThroughput);
        Ok(())
    }
}
