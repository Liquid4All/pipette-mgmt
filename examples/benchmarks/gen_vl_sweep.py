#!/usr/bin/env python3
"""Generate the VLM throughput and peak-memory benchmark sweep.

Sweeps image resolution, frame count, text prefill, and decode length.
Writes the benchmark TOMLs flat into the catalog dir. Run from
examples/benchmarks/: python3 gen_vl_sweep.py [--toml-dir .] [--json-dir <dir>]

Benchmark IDs spell out each axis: `text{N}` (text prefill tokens),
`decode{N}` (generated tokens, throughput only), `img{N}` (images packed into
one prompt; >1 = multi-frame / video).
"""

import argparse
import json
import os

DECODE = 64

# ---- vl_throughput points (width, height, text, decode, images) -----------
# resolution sweep (single image, no text): 256..1024 = no split, 1280/2048 = split
RES = [(w, w, 0, DECODE, 1) for w in (256, 512, 768, 1024, 1280, 2048)]
# frame / video sweep (256 px, no text)
FRAME = [(256, 256, 0, DECODE, images) for images in (1, 5, 16, 80)]
# text-prefill sweep (with vs without text), at no-split (512) and split (1280)
TEXT = [(w, w, text, DECODE, 1) for w in (512, 1280) for text in (0, 1024)]
# decode-length sweep (512 px, no text)
DEC = [(512, 512, 0, decode, 1) for decode in (16, 64, 256)]
THROUGHPUT = RES + FRAME + TEXT + DEC

# ---- vl_max_memory points (width, height, text, images) -------------------
# Peak memory is reported with a realistic text prefill (1024): the LLM-head
# KV cache is resident alongside the vision encoder, so text-bearing cells are
# the primary footprint measure. A couple of vision-only (text 0) cells stay as
# an isolation baseline for the encoder + image-token KV cost on its own.
MEM = [
    # vision-only baseline (isolate encoder + image-token KV)
    (256, 256, 0, 1),  # small single image
    (512, 512, 0, 1),  # single max-tile (no split)
    # realistic prefill: image + 1024 text tokens
    (256, 256, 1024, 1),  # small single image + text
    (512, 512, 1024, 1),  # single max-tile + text
    (1280, 1280, 1024, 1),  # tiled (split on) + text
    (256, 256, 1024, 5),  # multi-frame + text
]


def dedup(seq):
    seen, out = set(), []
    for p in seq:
        if p not in seen:
            seen.add(p)
            out.append(p)
    return out


def write(path_toml, path_json, toml, obj):
    with open(path_toml, "w") as fh:
        fh.write(toml)
    if path_json is not None:
        with open(path_json, "w") as fh:
            json.dump(obj, fh)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--toml-dir", default=".")
    ap.add_argument("--json-dir", default=None)
    a = ap.parse_args()
    os.makedirs(a.toml_dir, exist_ok=True)
    if a.json_dir:
        os.makedirs(a.json_dir, exist_ok=True)
    n = 0
    for width, height, text, decode, images in dedup(THROUGHPUT):
        bid = f"vl_throughput_{width}x{height}_text{text}_decode{decode}_img{images}"
        toml = (
            'benchmark_type          = "vl_throughput"\n'
            f"parameter_image_width   = {width}\nparameter_image_height  = {height}\n"
            f"parameter_text_tokens   = {text}\nparameter_decode_tokens = {decode}\n"
            f"parameter_num_images    = {images}\n"
        )
        obj = {
            "benchmark_type": "vl_throughput",
            "benchmark_id": bid,
            "parameter_image_width": width,
            "parameter_image_height": height,
            "parameter_text_tokens": text,
            "parameter_decode_tokens": decode,
            "parameter_num_images": images,
        }
        write(
            os.path.join(a.toml_dir, bid + ".toml"),
            os.path.join(a.json_dir, bid + ".json") if a.json_dir else None,
            toml,
            obj,
        )
        n += 1
    for width, height, text, images in dedup(MEM):
        bid = f"vl_max_memory_{width}x{height}_text{text}_img{images}"
        toml = (
            'benchmark_type          = "vl_max_memory"\n'
            f"parameter_image_width   = {width}\nparameter_image_height  = {height}\n"
            f"parameter_text_tokens   = {text}\nparameter_num_images    = {images}\n"
        )
        obj = {
            "benchmark_type": "vl_max_memory",
            "benchmark_id": bid,
            "parameter_image_width": width,
            "parameter_image_height": height,
            "parameter_text_tokens": text,
            "parameter_num_images": images,
        }
        write(
            os.path.join(a.toml_dir, bid + ".toml"),
            os.path.join(a.json_dir, bid + ".json") if a.json_dir else None,
            toml,
            obj,
        )
        n += 1
    print(
        f"generated {n} benchmarks -> toml:{a.toml_dir}"
        + (f" json:{a.json_dir}" if a.json_dir else "")
    )


if __name__ == "__main__":
    main()
