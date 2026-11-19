#!/usr/bin/env python3
"""sb ai fit against a fake model index: the quants, the KV cache, the verdict
for a machine, and the key hygiene. No network: every answer comes from a
recorded fixture or from a GGUF header built in this file."""

import json
import os
import socket
import struct
import subprocess
import sys
import tempfile
import threading
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from unittest import mock

CLI = Path(__file__).resolve().parents[1]
SB = CLI / "sb"
sys.path.insert(0, str(CLI))
import ai_fit  # noqa: E402  (the module sits next to the tests)

GB = 1_000_000_000
KEY = "hf_test_0f3a9c1d7e5b"

# ---------------------------------------------------------------- a GGUF header

def gguf_string(text):
    raw = text.encode("utf-8")
    return struct.pack("<Q", len(raw)) + raw


def gguf_value(kind, value):
    """One GGUF value: a string, a number, a bool or an array."""
    if kind == 8:                     # a string
        return gguf_string(value)
    if kind == 7:                     # a bool
        return struct.pack("<B", 1 if value else 0)
    if kind == 4:                     # a uint32
        return struct.pack("<I", value)
    if kind == 10:                    # a uint64
        return struct.pack("<Q", value)
    if kind == 6:                     # a float32
        return struct.pack("<f", value)
    if kind == 9:                     # an array: the type of the items, then the items
        element, items = value
        out = struct.pack("<I", element) + struct.pack("<Q", len(items))
        for item in items:
            out += gguf_value(element, item)
        return out
    raise ValueError(f"no value of type {kind}")


def gguf_header(keys, tensors):
    """A GGUF header: the magic, the counts, the key/value pairs, the tensors."""
    out = bytearray(b"GGUF")
    out += struct.pack("<I", 3)
    out += struct.pack("<Q", len(tensors))
    out += struct.pack("<Q", len(keys))
    for key, (kind, value) in keys.items():
        out += gguf_string(key)
        out += struct.pack("<I", kind)
        out += gguf_value(kind, value)
    for name, dims, kind in tensors:
        out += gguf_string(name)
        out += struct.pack("<I", len(dims))
        for dim in dims:
            out += struct.pack("<Q", dim)
        out += struct.pack("<I", kind)
        out += struct.pack("<Q", 0)
    return bytes(out)


def moe_header():
    """The header of a mixture of experts: forty one layers of which eleven
    keep a KV cache, and 256 experts of which 8 are used."""
    keys = {
        "general.architecture": (8, "testmoe"),
        "general.name": (8, "a mixture of experts"),
        "general.file_type": (4, 15),
        "testmoe.block_count": (4, 41),
        "testmoe.context_length": (4, 262144),
        "testmoe.embedding_length": (4, 2048),
        "testmoe.attention.head_count": (4, 16),
        "testmoe.attention.head_count_kv": (4, 2),
        "testmoe.attention.key_length": (4, 256),
        "testmoe.attention.value_length": (4, 256),
        "testmoe.expert_count": (4, 256),
        "testmoe.expert_used_count": (4, 8),
        "testmoe.full_attention_interval": (4, 4),
        # an array value: the layers that are not recurrent keep a cache
        "testmoe.attention.recurrent_layers": (
            9, (7, ([True, True, True, False] * 10) + [False])),
    }
    # the experts take 92.3% of the weights, like the model they stand for
    tensors = [
        ("blk.0.ffn_gate_exps.weight", [1000, 1000, 1670], 0),
        ("blk.0.ffn_up_exps.weight", [1000, 1000, 1670], 0),
        ("blk.0.ffn_down_exps.weight", [1000, 1000, 1670], 0),
        ("token_embd.weight", [1000, 1000, 209], 0),
        ("output.weight", [1000, 1000, 209], 0),
    ]
    return gguf_header(keys, tensors)


def dense_header(arch="testdense", layers=32):
    """The header of a plain model: every layer keeps a KV cache."""
    keys = {
        "general.architecture": (8, arch),
        "general.name": (8, "a plain model"),
        "general.file_type": (4, 15),
        f"{arch}.block_count": (4, layers),
        f"{arch}.context_length": (4, 131072),
        f"{arch}.embedding_length": (4, 4096),
        f"{arch}.attention.head_count": (4, 32),
        f"{arch}.attention.head_count_kv": (4, 8),
        f"{arch}.attention.key_length": (4, 128),
        f"{arch}.attention.value_length": (4, 128),
    }
    tensors = [
        ("token_embd.weight", [512, 512], 0),
        ("blk.0.attn_q.weight", [512, 512], 0),
    ]
    return gguf_header(keys, tensors)


def vision_header():
    """The header of a mixture of experts that looks at pictures too.

    Thirty layers, of which twenty-five keep a sliding window of 1024
    tokens and eight kv heads, and five keep the whole context with two kv
    heads. A hundred and twenty eight experts, of which eight are used.
    """
    keys = {
        "general.architecture": (8, "gemma4"),
        "general.name": (8, "a mixture of experts that looks at pictures"),
        "general.file_type": (4, 2),
        "gemma4.block_count": (4, 30),
        "gemma4.context_length": (4, 262144),
        "gemma4.embedding_length": (4, 2816),
        "gemma4.attention.head_count": (4, 16),
        "gemma4.attention.head_count_kv": (9, (4, ([8] * 5 + [2]) * 5)),
        "gemma4.attention.key_length": (4, 512),
        "gemma4.attention.value_length": (4, 512),
        "gemma4.attention.sliding_window": (4, 1024),
        "gemma4.attention.sliding_window_pattern": (9, (7, ([True] * 5 + [False]) * 5)),
        "gemma4.expert_count": (4, 128),
        "gemma4.expert_used_count": (4, 8),
    }
    # the experts take 90.3% of the weights, like the model they stand for
    tensors = [
        ("blk.0.ffn_gate_exps.weight", [1000, 1000, 1670], 0),
        ("blk.0.ffn_up_exps.weight", [1000, 1000, 1670], 0),
        ("blk.0.ffn_down_exps.weight", [1000, 1000, 1670], 0),
        ("token_embd.weight", [1000, 1000, 270], 0),
        ("output.weight", [1000, 1000, 270], 0),
    ]
    return gguf_header(keys, tensors)


def picture_header():
    """The header of a GGUF file that holds a picture: the architecture in
    the header says so."""
    keys = {
        "general.architecture": (8, "flux"),
        "general.name": (8, "a picture model"),
        "general.file_type": (4, 15),
    }
    return gguf_header(keys, [])


# ---------------------------------------------------------------- the fixtures

# A mixture of experts in gguf, with its projector.
MOE_LISTING = {
    "id": "test/moe-35b",
    "pipeline_tag": "text-generation",
    "siblings": [
        {"rfilename": "model-Q8_0.gguf", "size": 37_802_149_536},
        {"rfilename": "model-Q4_K_M.gguf", "size": 21_713_462_944},
        {"rfilename": "model-Q2_K.gguf", "size": 13_838_604_160},
        {"rfilename": "mmproj-model-F16.gguf", "size": 899_283_520},
        {"rfilename": "README.md", "size": 5424},
    ],
}

# A plain model whose file is too big for one piece, with a projector.
DENSE_LISTING = {
    "id": "test/dense-27b",
    "pipeline_tag": "text-generation",
    "siblings": [
        {"rfilename": "model-Q8_0.gguf", "size": 25_000_000_000},
        {"rfilename": "model-Q4_K_M-00001-of-00002.gguf", "size": 9_000_000_000},
        {"rfilename": "model-Q4_K_M-00002-of-00002.gguf", "size": 6_190_000_000},
        {"rfilename": "mmproj-model-f16.gguf", "size": 500_000_000},
        {"rfilename": "README.md", "size": 1200},
    ],
}

# A diffusion model: the parts sit in folders.
DIFFUSION_LISTING = {
    "id": "test/diffusion",
    "pipeline_tag": "text-to-video",
    "siblings": [
        {"rfilename": "diffusion_models/model.safetensors", "size": 12_000_000_000},
        {"rfilename": "text_encoders/encoder.safetensors", "size": 14_000_000_000},
        {"rfilename": "vae/vae.safetensors", "size": 1_000_000_000},
        {"rfilename": "loras/lora.safetensors", "size": 200_000_000},
        {"rfilename": "latent_upscale_models/up.safetensors", "size": 500_000_000},
    ],
}

# A text model that looks at pictures as well: the model sits at the root,
# the projectors sit at the root too and the draft heads have a folder of
# their own. The pipeline tag says it makes pictures.
VISION_LISTING = {
    "id": "test/vision-26b",
    "pipeline_tag": "image-text-to-text",
    "siblings": [
        {"rfilename": "gemma-4-26B-A4B-it-qat-UD-Q4_K_XL.gguf",
         "size": 14_249_047_104},
        {"rfilename": "mmproj-BF16.gguf", "size": 1_194_828_256},
        {"rfilename": "mmproj-F16.gguf", "size": 1_193_058_784},
        {"rfilename": "mmproj-F32.gguf", "size": 2_291_200_480},
        {"rfilename": "mtp-gemma-4-26B-A4B-it.gguf", "size": 251_939_328},
        {"rfilename": "MTP/mtp-gemma-4-26B-A4B-it-BF16.gguf", "size": 855_247_360},
        {"rfilename": "MTP/mtp-gemma-4-26B-A4B-it-F16.gguf", "size": 855_247_360},
        {"rfilename": "MTP/mtp-gemma-4-26B-A4B-it-Q4_0.gguf", "size": 251_939_328},
        {"rfilename": "MTP/mtp-gemma-4-26B-A4B-it-Q8_0.gguf", "size": 461_785_600},
        {"rfilename": "README.md", "size": 30935},
    ],
}

# A GGUF file that holds a picture: the architecture in its header says so.
PICTURE_LISTING = {
    "id": "test/picture",
    "pipeline_tag": "text-to-image",
    "siblings": [
        {"rfilename": "model-Q4_K_S.gguf", "size": 6_000_000_000},
    ],
}

# A transformers model: the sizes are in the listing, the shape in the config.
SAFETENSORS_LISTING = {
    "id": "test/safetensors",
    "pipeline_tag": "text-generation",
    "siblings": [
        {"rfilename": "model-00001-of-00002.safetensors", "size": 5_000_000_000},
        {"rfilename": "model-00002-of-00002.safetensors", "size": 2_000_000_000},
        {"rfilename": "config.json", "size": 1024},
    ],
}

SAFETENSORS_CONFIG = {
    "model_type": "testhybrid",
    "text_config": {
        "num_hidden_layers": 64,
        "num_attention_heads": 24,
        "num_key_value_heads": 4,
        "head_dim": 256,
        "max_position_embeddings": 262144,
        # three linear attention layers, then one that keeps a KV cache
        "layer_types": ["linear_attention", "linear_attention", "linear_attention",
                        "full_attention"] * 16,
    },
}

# The three models the speed was measured on.
DENSE_14B_LISTING = {
    "id": "calib/dense-14b",
    "pipeline_tag": "text-generation",
    "siblings": [{"rfilename": "model-Q4_K_M.gguf", "size": 8_990_000_000}],
}

DENSE_27B_LISTING = {
    "id": "calib/dense-27b",
    "pipeline_tag": "text-generation",
    "siblings": [{"rfilename": "model-Q4_K.gguf", "size": 15_190_000_000}],
}

CALIBRATION_MOE_LISTING = {
    "id": "calib/moe-35b",
    "pipeline_tag": "text-generation",
    "siblings": [{"rfilename": "model-Q4_K_M.gguf", "size": 21_713_462_944}],
}

# The same vision model without its projector and its draft heads: it is
# the one the speed of a unified memory machine was measured on.
CALIBRATION_VISION_LISTING = {
    "id": "calib/vision-26b",
    "pipeline_tag": "image-text-to-text",
    "siblings": [
        {"rfilename": "gemma-4-26B-A4B-it-qat-UD-Q4_K_XL.gguf",
         "size": 14_249_047_104},
    ],
}


class Index:
    """A model index that answers from the fixtures above."""

    def __init__(self, listings, configs, headers):
        self.listings = listings
        self.configs = configs
        self.headers = headers
        self.seen = []
        self.lock = threading.Lock()
        with socket.socket() as probe:
            probe.bind(("127.0.0.1", 0))
            self.port = probe.getsockname()[1]
        self.server = ThreadingHTTPServer(("127.0.0.1", self.port), self.handler())
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()

    @property
    def url(self):
        return f"http://127.0.0.1:{self.port}"

    def close(self):
        self.server.shutdown()
        self.server.server_close()

    def handler(self):
        index = self

        class Handler(BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def log_message(self, *args):
                pass

            def do_GET(self):
                index.serve(self)

        return Handler

    def serve(self, handler):
        path = handler.path
        query = ""
        if "?" in path:
            path, query = path.split("?", 1)
        with self.lock:
            self.seen.append({"path": path, "query": query,
                              "auth": handler.headers.get("Authorization")})
        if path.startswith("/api/models/"):
            listing = self.listings.get(path[len("/api/models/"):])
            if listing is None:
                handler.send_error(404)
                return
            self.send(handler, 200, json.dumps(listing).encode(), "application/json")
            return
        if "/resolve/main/" in path:
            repo, name = path.split("/resolve/main/", 1)
            repo = repo.strip("/")
            if name == "config.json":
                config = self.configs.get(repo)
                if config is None:
                    handler.send_error(404)
                    return
                self.send(handler, 200, json.dumps(config).encode(), "application/json")
                return
            body = self.headers.get((repo, name))
            if body is None:
                handler.send_error(404)
                return
            self.send_bytes(handler, body)
            return
        handler.send_error(404)

    def send(self, handler, status, payload, content_type):
        handler.send_response(status)
        handler.send_header("Content-Type", content_type)
        handler.send_header("Content-Length", str(len(payload)))
        handler.end_headers()
        handler.wfile.write(payload)

    def send_bytes(self, handler, body):
        """A file, or the part of it that a range asks for."""
        start, end = 0, len(body) - 1
        status = 200
        span = handler.headers.get("Range")
        if span and span.startswith("bytes="):
            first, _, last = span[len("bytes="):].partition("-")
            start = int(first)
            end = int(last) if last else len(body) - 1
            end = min(end, len(body) - 1)
            status = 206
        payload = body[start:end + 1]
        handler.send_response(status)
        handler.send_header("Content-Type", "application/octet-stream")
        handler.send_header("Accept-Ranges", "bytes")
        if status == 206:
            handler.send_header("Content-Range", f"bytes {start}-{end}/{len(body)}")
        handler.send_header("Content-Length", str(len(payload)))
        handler.end_headers()
        handler.wfile.write(payload)


class Redirects:
    """Two servers on the same address.

    The first one answers 302 and sends the request on: to the second
    server, which sits on another port and is therefore another host, or
    back to itself. Both remember what they were sent.
    """

    def __init__(self):
        self.seen = []
        self.lock = threading.Lock()
        self.ports = []
        for _ in range(2):
            with socket.socket() as probe:
                probe.bind(("127.0.0.1", 0))
                self.ports.append(probe.getsockname()[1])
        self.servers = []
        for index in range(2):
            server = ThreadingHTTPServer(("127.0.0.1", self.ports[index]),
                                         self.handler(index))
            self.servers.append(server)
            threading.Thread(target=server.serve_forever, daemon=True).start()

    def url(self, index, path=""):
        return f"http://127.0.0.1:{self.ports[index]}{path}"

    def close(self):
        for server in self.servers:
            server.shutdown()
            server.server_close()

    def handler(self, index):
        pair = self

        class Handler(BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def log_message(self, *args):
                pass

            def do_GET(self):
                pair.serve(self, index)

        return Handler

    def serve(self, handler, index):
        with self.lock:
            self.seen.append({"server": index, "path": handler.path,
                              "auth": handler.headers.get("Authorization")})
        if handler.path == "/away":
            target = f"http://127.0.0.1:{self.ports[1]}/target"
        elif handler.path == "/back":
            target = f"http://127.0.0.1:{self.ports[0]}/target"
        else:
            self.send(handler, b'{"ok": true}')
            return
        handler.send_response(302)
        handler.send_header("Location", target)
        handler.send_header("Content-Length", "0")
        handler.end_headers()

    def send(self, handler, payload):
        handler.send_response(200)
        handler.send_header("Content-Type", "application/json")
        handler.send_header("Content-Length", str(len(payload)))
        handler.end_headers()
        handler.wfile.write(payload)


# ---------------------------------------------------------------- the tests

def within_twice(measured, predicted):
    """Is the estimate within a factor of two of what was measured?"""
    return measured / 2 <= predicted <= measured * 2


def within_twice_of_range(predicted, low, high):
    """Is the estimate within a factor of two of a measured range?"""
    return predicted / 2 <= high and predicted * 2 >= low


class SbAiFit(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix="sb-ai-fit-")
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name).resolve()
        (self.root / "home").mkdir()
        (self.root / "runtime/config").mkdir(parents=True)
        (self.root / "runtime/manifest.json").write_text("{}")
        listings = {
            "test/moe-35b": MOE_LISTING,
            "test/dense-27b": DENSE_LISTING,
            "test/diffusion": DIFFUSION_LISTING,
            "test/safetensors": SAFETENSORS_LISTING,
            "test/vision-26b": VISION_LISTING,
            "test/picture": PICTURE_LISTING,
            "calib/dense-14b": DENSE_14B_LISTING,
            "calib/dense-27b": DENSE_27B_LISTING,
            "calib/moe-35b": CALIBRATION_MOE_LISTING,
            "calib/vision-26b": CALIBRATION_VISION_LISTING,
        }
        headers = {}
        for repo, listing in listings.items():
            if "vision" in repo:
                body = vision_header()
            elif "picture" in repo:
                body = picture_header()
            elif "moe" in repo:
                body = moe_header()
            else:
                body = dense_header()
            for sibling in listing["siblings"]:
                if sibling["rfilename"].endswith(".gguf"):
                    headers[(repo, sibling["rfilename"])] = body
        self.index = Index(listings, {"test/safetensors": SAFETENSORS_CONFIG}, headers)
        self.addCleanup(self.index.close)
        self.env = {
            "PATH": os.environ.get("PATH", "/usr/bin:/bin"),
            "HOME": str(self.root / "home"),
            "SWITCHBACK_RUNTIME_ROOT": str(self.root / "runtime"),
            "SB_AI_FIT_HF_BASE": self.index.url,
            "NO_PROXY": "127.0.0.1",
            "no_proxy": "127.0.0.1",
        }

    def fit(self, *args, env=None, timeout=60):
        run = subprocess.run(["zsh", str(SB), "ai", "fit", *args], env=env or self.env,
                             cwd=self.root, capture_output=True, text=True, timeout=timeout)
        self.assertNotIn(KEY, run.stdout)
        self.assertNotIn(KEY, run.stderr)
        return run

    def answer(self, *args, **kwargs):
        """The json answer of a repository."""
        run = self.fit(*args, "--json", **kwargs)
        self.assertEqual(run.returncode, 0, run.stderr)
        return json.loads(run.stdout)

    # ------------------------------------------------------------ the listing

    def test_the_quants_are_listed_with_their_sizes(self):
        run = self.fit("test/moe-35b")
        self.assertEqual(run.returncode, 0, run.stderr)
        self.assertIn("Q4_K_M", run.stdout)
        self.assertIn("21.71 GB", run.stdout)
        self.assertIn("projector", run.stdout)
        self.assertIn("mmproj-model-F16.gguf", run.stdout)

    def test_the_kv_cache_comes_from_the_header(self):
        report = self.answer("test/moe-35b")
        self.assertEqual(report["kind"], "gguf")
        self.assertEqual(report["model"]["layers"], 41)
        self.assertEqual(report["model"]["kv_layers"], 11)
        self.assertEqual(report["model"]["kv_heads"], 2)
        self.assertEqual(report["model"]["head_dim"], 256)
        self.assertEqual(report["model"]["experts"], 256)
        self.assertEqual(report["model"]["experts_used"], 8)
        # eleven layers, two kv heads, 256 wide, an f16 cache at 32768 tokens
        self.assertEqual(report["kv_cache"]["bytes"], 11 * 32768 * 2 * 2 * (256 + 256))
        self.assertTrue(report["kv_cache"]["known"])

    def test_the_split_files_are_one_model(self):
        report = self.answer("test/dense-27b")
        quants = {option["quant"]: option for option in report["options"]}
        self.assertIn("Q4_K_M", quants)
        self.assertEqual(quants["Q4_K_M"]["bytes"], 9_000_000_000 + 6_190_000_000)
        self.assertEqual(len(quants["Q4_K_M"]["files"]), 2)
        self.assertIn("model-Q4_K_M-00001-of-00002.gguf",
                      [file["name"] for file in quants["Q4_K_M"]["files"]])

    # ------------------------------------------------------------ the verdict

    def test_the_fit_takes_the_best_quant_that_fits(self):
        report = self.answer("test/moe-35b", "--vram", "32", "--ram", "62",
                              "--bandwidth", "1008")
        machine = report["machines"][0]
        self.assertEqual(machine["verdict"], "fits")
        self.assertEqual(machine["quant"], "Q4_K_M")
        # the weights, the cache, the projector and the compute margin
        self.assertEqual(machine["needed_bytes"],
                         21_713_462_944 + report["kv_cache"]["bytes"] + 899_283_520
                         + 1_500_000_000)
        self.assertLessEqual(machine["needed_bytes"], 32 * GB)
        self.assertEqual(machine["disk_bytes"], 21_713_462_944 + 899_283_520)

    def test_a_model_that_does_not_fit_is_offloaded_and_says_so(self):
        report = self.answer("test/dense-27b", "--vram", "12", "--ram", "64",
                              "--bandwidth", "1008")
        machine = report["machines"][0]
        self.assertEqual(machine["verdict"], "partial")
        self.assertGreater(machine["offloaded_bytes"], 0)
        self.assertEqual(machine["on_gpu_bytes"] + machine["offloaded_bytes"],
                         machine["weights_bytes"])
        self.assertTrue(any("order of magnitude" in word
                            for word in machine["warnings"]), machine["warnings"])
        # the smaller file writes faster, and the answer says so
        self.assertEqual(machine["faster_alternative"]["quant"], "Q4_K_M")
        self.assertGreater(machine["faster_alternative"]["speed_estimate_tok_s"],
                           machine["speed_estimate_tok_s"])

    def test_a_model_that_cannot_fit_at_all_says_why(self):
        report = self.answer("test/dense-27b", "--vram", "4", "--ram", "64")
        machine = report["machines"][0]
        self.assertEqual(machine["verdict"], "no-fit")
        self.assertTrue(machine["warnings"], machine)
        self.assertIn("KV cache", machine["warnings"][0])

    def test_without_a_machine_only_the_sizes_and_the_cache_are_shown(self):
        run = self.fit("test/moe-35b")
        self.assertEqual(run.returncode, 0, run.stderr)
        self.assertIn("kv cache at 32768 tokens", run.stdout)
        self.assertNotIn("tok/s", run.stdout)
        self.assertEqual(self.answer("test/moe-35b")["machines"], [])

    def test_the_machine_file_gives_several_machines(self):
        path = self.root / "machines.json"
        path.write_text(json.dumps({"machines": [
            {"name": "box", "kind": "gpu", "vram_gb": 32, "ram_gb": 62,
             "bandwidth_gbs": 1008},
            {"name": "laptop", "kind": "unified", "usable_gb": 28,
             "bandwidth_gbs": 546},
        ]}))
        report = self.answer("test/moe-35b", "--machines", str(path))
        self.assertEqual([machine["name"] for machine in report["machines"]],
                         ["box", "laptop"])
        self.assertEqual(report["machines"][1]["kind"], "unified")

    def test_the_diffusion_parts_are_listed_and_the_big_one_is_offloaded(self):
        report = self.answer("test/diffusion", "--vram", "24", "--ram", "62")
        self.assertEqual(report["kind"], "diffusion")
        self.assertEqual({part["folder"] for part in report["components"]},
                         {"diffusion_models", "text_encoders", "vae", "loras",
                          "latent_upscale_models"})
        machine = report["machines"][0]
        self.assertEqual(machine["verdict"], "fits")
        self.assertEqual(machine["model"]["name"], "diffusion_models/model.safetensors")
        # the text encoder does not fit next to the model, so it is offloaded
        self.assertEqual([part["name"] for part in machine["offload"]],
                         ["text_encoders/encoder.safetensors"])
        self.assertNotIn("speed_estimate_tok_s", machine)

    def test_the_transformers_layout_reads_its_cache_from_the_config(self):
        report = self.answer("test/safetensors", "--vram", "24", "--ram", "62")
        self.assertEqual(report["kind"], "safetensors")
        self.assertEqual(report["model"]["layers"], 64)
        self.assertEqual(report["model"]["kv_layers"], 16)
        self.assertEqual(report["kv_cache"]["bytes"], 16 * 32768 * 2 * 4 * (256 + 256))
        machine = report["machines"][0]
        self.assertEqual(machine["verdict"], "fits")
        self.assertEqual(machine["weights_bytes"], 7_000_000_000)

    # ------------------------------------------------------------ the vision model

    def test_a_model_that_looks_at_pictures_is_still_a_text_model(self):
        """The pipeline tag of a vision model says it makes pictures too, but
        the file is a language model: the header of the file decides."""
        report = self.answer("test/vision-26b", "--vram", "24", "--ram", "62",
                             "--bandwidth", "1008")
        self.assertEqual(report["kind"], "gguf")
        self.assertEqual(report["model"]["architecture"], "gemma4")
        self.assertEqual(report["model"]["layers"], 30)
        self.assertEqual(report["model"]["experts"], 128)
        self.assertEqual(report["model"]["experts_used"], 8)
        # the projector and the draft heads go with the model, they are not
        # the model
        self.assertEqual([option["quant"] for option in report["options"]],
                         ["Q4_K_XL"])
        self.assertEqual([file["name"] for file in report["options"][0]["files"]],
                         ["gemma-4-26B-A4B-it-qat-UD-Q4_K_XL.gguf"])
        self.assertEqual(len(report["projector"]["files"]), 3)
        self.assertEqual(len(report["draft"]["files"]), 5)
        machine = report["machines"][0]
        self.assertEqual(machine["verdict"], "fits")
        # A server loads one projector, not all three precisions.
        self.assertEqual(report["projector"]["chosen"], "mmproj-F16.gguf")
        self.assertEqual(report["projector"]["bytes"], 1_193_058_784)
        self.assertEqual(machine["projector_bytes"], 1_193_058_784)
        self.assertTrue(machine["speed_estimate_tok_s"])

    def test_the_cache_follows_the_layers_of_the_header(self):
        """Twenty-five layers keep a sliding window of 1024 tokens and eight
        kv heads, five keep the whole context and two kv heads."""
        report = self.answer("test/vision-26b")
        sliding = 25 * 1024 * 2 * 8 * (512 + 512)
        full = 5 * 32768 * 2 * 2 * (512 + 512)
        self.assertEqual(report["kv_cache"]["bytes"], sliding + full)
        self.assertTrue(report["kv_cache"]["known"])
        self.assertEqual(report["model"]["sliding_window"], 1024)
        self.assertEqual(report["model"]["sliding_layers"], 25)

    def test_a_file_that_holds_a_picture_is_a_diffusion_model(self):
        """The header names a picture architecture, so the repository is
        measured by its parts."""
        report = self.answer("test/picture", "--vram", "24", "--ram", "62")
        self.assertEqual(report["kind"], "diffusion")
        self.assertEqual([part["name"] for part in report["components"]],
                         ["model-Q4_K_S.gguf"])
        self.assertEqual(report["machines"][0]["verdict"], "fits")

    # ------------------------------------------------------------ the key

    def test_the_token_is_never_printed(self):
        env = dict(self.env, HF_TOKEN=KEY)
        run = self.fit("test/moe-35b", env=env)
        self.assertEqual(run.returncode, 0, run.stderr)
        self.assertNotIn(KEY, run.stdout)
        self.assertNotIn(KEY, run.stderr)
        # it went in the header, and nowhere else
        self.assertIn(f"Bearer {KEY}",
                      [seen["auth"] for seen in self.index.seen])

    def test_the_token_is_not_in_an_error(self):
        env = dict(self.env, HF_TOKEN=KEY)
        run = self.fit("test/nothing-here", env=env)
        self.assertEqual(run.returncode, 1)
        self.assertNotIn(KEY, run.stdout)
        self.assertNotIn(KEY, run.stderr)
        self.assertIn("not found", run.stderr)

    def test_the_token_does_not_go_to_another_host(self):
        """A file of a model index is served from another host: the token
        stays where it was sent."""
        servers = Redirects()
        self.addCleanup(servers.close)
        with mock.patch.dict(os.environ, {"no_proxy": "127.0.0.1",
                                          "NO_PROXY": "127.0.0.1"}):
            answer = ai_fit.fetch_json(servers.url(0, "/away"), KEY, "the answer")
        self.assertEqual(answer, {"ok": True})
        first = [seen for seen in servers.seen if seen["server"] == 0]
        self.assertEqual(first[0]["auth"], f"Bearer {KEY}")
        second = [seen for seen in servers.seen if seen["server"] == 1]
        self.assertTrue(second, servers.seen)
        self.assertIsNone(second[0]["auth"], servers.seen)

    def test_the_token_stays_on_the_same_host(self):
        """A redirect to the same host keeps the token: the file is still
        served by the machine it was asked for."""
        servers = Redirects()
        self.addCleanup(servers.close)
        with mock.patch.dict(os.environ, {"no_proxy": "127.0.0.1",
                                          "NO_PROXY": "127.0.0.1"}):
            ai_fit.fetch_json(servers.url(0, "/back"), KEY, "the answer")
        target = [seen for seen in servers.seen if seen["path"] == "/target"]
        self.assertTrue(target, servers.seen)
        self.assertEqual(target[0]["auth"], f"Bearer {KEY}")

    # ------------------------------------------------------------ the parser

    def test_the_gguf_parser_reads_a_string_an_array_and_the_tensors(self):
        keys, tensors = ai_fit.parse_gguf(moe_header(), True, None)
        self.assertEqual(keys["general.architecture"], "testmoe")
        self.assertEqual(keys["general.name"], "a mixture of experts")
        self.assertEqual(keys["testmoe.block_count"], 41)
        self.assertEqual(keys["testmoe.expert_count"], 256)
        recurrent = keys["testmoe.attention.recurrent_layers"]
        self.assertEqual(len(recurrent), 41)
        self.assertEqual(recurrent.count(False), 11)
        self.assertEqual(len(tensors), 5)
        self.assertEqual(tensors[0].name, "blk.0.ffn_gate_exps.weight")
        # the experts take 92.3% of the weights
        self.assertAlmostEqual(ai_fit.expert_share(tensors),
                               20_040_000_000 / 21_712_000_000, places=6)

    def test_the_parser_stops_at_the_keys_it_was_asked_for(self):
        keys, tensors = ai_fit.parse_gguf(moe_header(), False, {"general.architecture"})
        self.assertEqual(list(keys), ["general.architecture"])
        self.assertEqual(tensors, [])

    # ------------------------------------------------------------ the speed

    def test_the_speed_matches_the_numbers_that_were_measured(self):
        """The estimate, against the machines it was measured on.

        A dense 14B at Q4_K_M wrote about 93 tokens a second on a 24 GB GPU
        with 1008 GB/s. A 35B mixture of experts at about 4.5 bits wrote
        125 to 170 there. A dense 27B at about 4.5 bits wrote about 48. A
        mixture of experts of 26B with 4B active, at about 4.8 bits, wrote
        116 to 118 on a 546 GB/s unified memory machine. Every one of them
        has to land within a factor of two.
        """
        gpu = ["--vram", "24", "--ram", "62", "--bandwidth", "1008"]
        dense14 = self.answer("calib/dense-14b", *gpu)["machines"][0]
        self.assertTrue(within_twice(93, dense14["speed_estimate_tok_s"]),
                        dense14["speed_estimate_tok_s"])
        dense27 = self.answer("calib/dense-27b", *gpu)["machines"][0]
        self.assertTrue(within_twice(48, dense27["speed_estimate_tok_s"]),
                        dense27["speed_estimate_tok_s"])
        mixture = self.answer("calib/moe-35b", *gpu)["machines"][0]
        self.assertTrue(within_twice_of_range(mixture["speed_estimate_tok_s"],
                                              125, 170),
                        mixture["speed_estimate_tok_s"])
        unified = self.answer("calib/vision-26b", "--ram", "28",
                              "--bandwidth", "546")["machines"][0]
        self.assertTrue(within_twice(117, unified["speed_estimate_tok_s"]),
                        unified["speed_estimate_tok_s"])

    def test_the_json_answer_has_the_documented_keys(self):
        report = self.answer("test/moe-35b", "--vram", "24")
        self.assertEqual(report["repo"], "test/moe-35b")
        self.assertEqual(report["kind"], "gguf")
        self.assertEqual(report["context_length"], 32768)
        for key in ("architecture", "layers", "kv_layers", "kv_heads",
                    "kv_heads_per_layer", "head_dim",
                    "value_dim", "experts", "experts_used", "max_context_length",
                    "sliding_window", "sliding_layers", "source"):
            self.assertIn(key, report["model"])
        for key in ("bytes", "known", "element_bytes", "note"):
            self.assertIn(key, report["kv_cache"])
        for key in ("quant", "files", "bytes", "quality_bits", "bytes_per_token",
                    "bytes_per_token_source"):
            self.assertIn(key, report["options"][0])
        for key in ("files", "bytes"):
            self.assertIn(key, report["projector"])
            self.assertIn(key, report["draft"])
        for key in ("name", "kind", "budget_bytes", "host_bytes", "bandwidth_gbs",
                    "verdict", "warnings"):
            self.assertIn(key, report["machines"][0])


if __name__ == "__main__":
    unittest.main()
