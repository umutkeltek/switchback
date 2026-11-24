#!/usr/bin/env python3
"""sb ai fit: where a model fits and which file to take.

One command answers the question that comes before every install: on this
machine, which file of this repository fits, and what does it cost? It reads
the Hugging Face metadata (file names and sizes), reads the header of one
GGUF file when the repository has one, and says which quant fits, how much
KV cache it needs, what has to be offloaded, how much disk it takes and
roughly how fast it would write.

Read-only: no model file is downloaded (only the metadata and one header),
nothing is written anywhere, and the gateway is never contacted.

Standard library only.
"""

import argparse
import concurrent.futures as futures
import json
import os
import re
import struct
import sys
import urllib.error
import urllib.parse
import urllib.request

OK, ERROR, USAGE, UNREACHABLE = 0, 1, 2, 3

# The base of the model index. The environment variable exists so a test can
# point the command at a local server; people leave it alone.
HF_BASE = os.environ.get("SB_AI_FIT_HF_BASE", "https://huggingface.co").rstrip("/")

DEFAULT_CTX = 32768
GB = 1_000_000_000
MARGIN_BYTES = 1_500_000_000   # room for the compute buffers of the runtime
CACHE_ELEMENT_BYTES = 2         # an f16 KV cache
# Bytes per KV cache element of each cache type (llama.cpp block sizes).
CACHE_TYPE_BYTES = {"f32": 4.0, "f16": 2.0, "bf16": 2.0, "q8_0": 34 / 32, "q5_1": 24 / 32,
                    "q5_0": 22 / 32, "q4_1": 20 / 32, "q4_0": 18 / 32, "iq4_nl": 18 / 32}
# How much of the stated memory bandwidth a decode really gets. The 0.55 is
# calibrated on two kinds of machine: a discrete 24 GB GPU, where a dense 14B
# Q4_K_M wrote about 93 tok/s, and a unified memory machine of 546 GB/s,
# where a mixture of experts of 26B with 4B active, at Q4_K_XL, wrote 116
# to 118 tok/s. One factor fits every measurement on both. An earlier note
# said unified memory measures far lower; that number came from a run at
# low priority and is not what the machine does.
FACTOR = 0.55
# Weights that live in host memory are read over the bus, not over the local
# memory: a partial offload is an order of magnitude slower.
OFFLOAD_PENALTY = 10.0
# When the KV cache cannot be worked out, this share of the weights is
# reserved for it, so a model that does not fit is not called a model that
# fits. It is stated wherever it is used.
UNKNOWN_KV_SHARE = 0.10
# How much of a GGUF header to read. The header of a large vocabulary model
# holds the tokenizer and runs to a few megabytes.
HEADER_FIRST_WINDOW = 64 * 1024
HEADER_LIMIT = 96 * 1024 * 1024
# The folders a ComfyUI style repository keeps its parts in.
COMPONENT_FOLDERS = ("diffusion_models", "text_encoders", "vae", "loras",
                      "latent_upscale_models", "unet", "clip")
# The folders that hold the model itself; the rest hold the parts that go
# with it.
MODEL_FOLDERS = ("diffusion_models", "unet")
AUX_FOLDERS = ("text_encoders", "vae", "loras", "latent_upscale_models", "clip")
# The architectures a GGUF file uses when it holds a picture rather than
# words. The header of the file says which one it is.
DIFFUSION_ARCHITECTURES = ("stable-diffusion", "stable_diffusion", "sd",
                           "sd3", "sdxl", "flux", "flux1")
# The jobs of a model that read text. Anything else is a picture, a video
# or a sound, and those are measured by their parts. The tag of a
# repository that holds GGUF files is not asked: the header of the file
# says what the model is.
TEXT_TAGS = ("text-generation", "text2text-generation", "text-to-text", "translation",
              "summarization", "question-answering", "fill-mask", "sentence-similarity",
              "text-classification", "token-classification", "multiple-choice",
              "feature-extraction", "zero-shot-classification", "table-to-text",
              "text-ranking", "code-generation", "conversational")
# What is kept in memory first when a diffusion repository does not fit.
COMPONENT_ORDER = {"vae": 0, "loras": 1, "latent_upscale_models": 2,
                   "text_encoders": 3, "clip": 3}
MODEL_SUFFIXES = (".gguf", ".safetensors", ".ckpt", ".pt", ".pth", ".bin", ".sft")


class FitError(Exception):
    """A stop with an exit code and one plain message."""

    def __init__(self, code, message, status=None):
        super().__init__(message)
        self.code = code
        self.message = message
        self.status = status


# ---------------------------------------------------------------- the model index

def read_token(environ):
    """The Hugging Face token, if there is one. It is never printed."""
    token = (environ.get("HF_TOKEN") or "").strip()
    if token:
        return token
    path = os.path.join(os.path.expanduser("~"), ".cache", "huggingface", "token")
    try:
        with open(path, encoding="utf-8") as handle:
            return handle.read().strip()
    except OSError:
        return ""


def repo_id(text):
    """`owner/name` or a huggingface.co link, as `owner/name`."""
    text = (text or "").strip()
    if text.startswith("http://") or text.startswith("https://"):
        parts = urllib.parse.urlsplit(text)
        if parts.netloc not in ("huggingface.co", "www.huggingface.co"):
            raise FitError(USAGE, "only huggingface.co repositories are read")
        path = [piece for piece in parts.path.split("/") if piece]
        if len(path) < 2:
            raise FitError(USAGE, "a huggingface.co link needs an owner and a name")
        text = "/".join(path[:2])
    if text.count("/") != 1 or not all(piece.strip() for piece in text.split("/")):
        raise FitError(USAGE, "a repository is owner/name, or a huggingface.co link")
    return text


def host_of(url):
    """The scheme, the host and the port of a URL: the three things that
    have to stay the same for a token to go along with a redirect."""
    parts = urllib.parse.urlsplit(url)
    scheme = parts.scheme.lower()
    host = (parts.hostname or "").lower()
    try:
        port = parts.port
    except ValueError:
        port = -1          # a port that cannot be read is not the same host
    if port is None:
        port = {"http": 80, "https": 443}.get(scheme)
    return scheme, host, port


class SafeRedirect(urllib.request.HTTPRedirectHandler):
    """A redirect that keeps the token to the host it was meant for.

    The default handler copies the Authorization header to the host the
    redirect points at. A file of a model index is served from another
    host, and the token does not belong there.
    """

    def redirect_request(self, req, fp, code, msg, headers, newurl):
        new = super().redirect_request(req, fp, code, msg, headers, newurl)
        if new is None:
            return None
        if host_of(req.full_url) != host_of(newurl):
            for key in [key for key in new.headers if key.lower() == "authorization"]:
                del new.headers[key]
        return new


def _open(url, token, what, headers=None, timeout=60):
    """Open a URL. The token goes in the Authorization header and nowhere else."""
    head = {"Accept": "*/*"}
    if token:
        head["Authorization"] = "Bearer " + token
    if headers:
        head.update(headers)
    request = urllib.request.Request(url, headers=head)
    opener = urllib.request.build_opener(SafeRedirect())
    try:
        return opener.open(request, timeout=timeout)
    except urllib.error.HTTPError as error:
        status = error.code
        if status == 404:
            raise FitError(ERROR, f"{what}: not found (404)", status) from None
        if status in (401, 403):
            raise FitError(ERROR, f"{what}: the repository is private or gated; "
                                   "set HF_TOKEN to read it", status) from None
        raise FitError(ERROR, f"{what}: the server answered {status}", status) from None
    except urllib.error.URLError as error:
        raise FitError(UNREACHABLE,
                       f"{what}: cannot reach the model index ({error.reason})") from None
    except OSError as error:
        raise FitError(UNREACHABLE,
                       f"{what}: cannot reach the model index ({error})") from None


def fetch_json(url, token, what, timeout=30):
    with _open(url, token, what, timeout=timeout) as response:
        raw = response.read(16 * 1024 * 1024)
    try:
        return json.loads(raw.decode("utf-8"))
    except (ValueError, UnicodeDecodeError):
        raise FitError(ERROR, f"{what}: the answer is not JSON") from None


def fetch_bytes(url, token, what, timeout=30):
    """The bytes of a file, or None when it is not there."""
    try:
        with _open(url, token, what, timeout=timeout) as response:
            return response.read(16 * 1024 * 1024)
    except FitError as error:
        if error.status == 404:
            return None
        raise


def fetch_range(url, token, start, end, what, timeout=60):
    """The bytes between two offsets. The range is honoured, so a header
    costs a header and not a model."""
    headers = {"Range": f"bytes={start}-{end}"}
    try:
        with _open(url, token, what, headers=headers, timeout=timeout) as response:
            return response.read(end - start + 1)
    except FitError as error:
        if error.status == 416:
            return b""   # the file ends before this range
        raise


# ---------------------------------------------------------------- the GGUF header

# The ggml types a GGUF tensor can hold: how many elements share a block and
# how many bytes that block takes.
GGML_TYPES = {
    0: (1, 4), 1: (1, 2), 2: (32, 18), 3: (32, 20), 6: (32, 22), 7: (32, 24),
    8: (32, 34), 9: (32, 36), 10: (256, 84), 11: (256, 110), 12: (256, 144),
    13: (256, 176), 14: (256, 210), 15: (256, 292), 16: (256, 66),
    17: (256, 74), 18: (256, 98), 19: (256, 50), 20: (32, 18), 21: (256, 110),
    22: (256, 82), 23: (256, 136), 24: (1, 1), 25: (1, 2), 26: (1, 4),
    27: (1, 8), 28: (1, 8), 29: (256, 56), 30: (1, 2), 34: (256, 54),
    35: (256, 66), 39: (32, 17),
}


class NeedMore(Exception):
    """The buffer does not hold the whole header yet."""


class Reader:
    """The GGUF header, read byte by byte from the front of a file."""

    def __init__(self, data):
        self.data = data
        self.pos = 0

    def take(self, count):
        end = self.pos + count
        if end > len(self.data):
            raise NeedMore(count)
        out = self.data[self.pos:end]
        self.pos = end
        return out

    def u8(self):
        return self.take(1)[0]

    def u16(self):
        return struct.unpack("<H", self.take(2))[0]

    def u32(self):
        return struct.unpack("<I", self.take(4))[0]

    def u64(self):
        return struct.unpack("<Q", self.take(8))[0]

    def i8(self):
        return struct.unpack("<b", self.take(1))[0]

    def i16(self):
        return struct.unpack("<h", self.take(2))[0]

    def i32(self):
        return struct.unpack("<i", self.take(4))[0]

    def i64(self):
        return struct.unpack("<q", self.take(8))[0]

    def f32(self):
        return struct.unpack("<f", self.take(4))[0]

    def f64(self):
        return struct.unpack("<d", self.take(8))[0]

    def string(self):
        length = self.u64()
        if length > 1 << 26:
            raise NeedMore(0)
        return self.take(length).decode("utf-8", "replace")

    def value(self, kind):
        if kind == 0:
            return self.u8()
        if kind == 1:
            return self.i8()
        if kind == 2:
            return self.u16()
        if kind == 3:
            return self.i16()
        if kind == 4:
            return self.u32()
        if kind == 5:
            return self.i32()
        if kind == 6:
            return self.f32()
        if kind == 7:
            return bool(self.u8())
        if kind == 8:
            return self.string()
        if kind == 9:
            element = self.u32()
            count = self.u64()
            return [self.value(element) for _ in range(count)]
        if kind == 10:
            return self.u64()
        if kind == 11:
            return self.i64()
        if kind == 12:
            return self.f64()
        raise FitError(ERROR, f"the GGUF header holds an unknown value type {kind}")


class Tensor:
    """One tensor of a GGUF file: its name, its shape and its type."""

    def __init__(self, name, dims, kind):
        self.name = name
        self.dims = dims
        self.kind = kind

    def bytes(self):
        """How many bytes the tensor takes."""
        entry = GGML_TYPES.get(self.kind)
        if not entry:
            return 0
        block, size = entry
        count = 1
        for dim in self.dims:
            count *= dim
        blocks = (count + block - 1) // block
        return blocks * size


def parse_gguf(data, want_tensors=True, wanted=None):
    """Read a GGUF header.

    `wanted` ends the key/value pass as soon as every key in it has been
    read, which keeps the tokenizer of a large vocabulary out of the way.
    """
    reader = Reader(data)
    if reader.take(4) != b"GGUF":
        raise FitError(ERROR, "the file does not start with a GGUF header")
    version = reader.u32()
    if version not in (2, 3):
        raise FitError(ERROR, f"the GGUF version {version} is not read")
    tensor_count = reader.u64()
    key_count = reader.u64()
    keys = {}
    for _ in range(key_count):
        if wanted is not None and wanted.issubset(keys.keys()):
            break
        key = reader.string()
        keys[key] = reader.value(reader.u32())
    if not want_tensors:
        return keys, []
    tensors = []
    for _ in range(tensor_count):
        name = reader.string()
        dims = [reader.u64() for _ in range(reader.u32())]
        tensors.append(Tensor(name, dims, reader.u32()))
        reader.u64()
    return keys, tensors


def expert_share(tensors):
    """The share of the file that the experts take, from the tensor sizes.

    The experts of a layer are stacked in one tensor whose name ends in
    `exps`; everything else is read for every token.
    """
    total = 0
    experts = 0
    for tensor in tensors:
        size = tensor.bytes()
        total += size
        if "exps" in tensor.name:
            experts += size
    if not total:
        return None
    return experts / total


def wanted_keys(arch):
    """The keys of a GGUF header that say how the model is put together."""
    keys = {"general.architecture", "general.file_type"}
    if not arch:
        return keys
    for name in ("block_count", "context_length", "embedding_length",
                 "feed_forward_length", "expert_count", "expert_used_count",
                 "expert_feed_forward_length", "expert_shared_feed_forward_length",
                 "full_attention_interval", "nextn_predict_layers",
                 "attention.head_count", "attention.head_count_kv",
                 "attention.key_length", "attention.value_length",
                 "attention.recurrent_layers", "attention.sliding_window",
                 "attention.sliding_window_pattern"):
        keys.add(f"{arch}.{name}")
    return keys


class Header:
    """The header of one GGUF file, read over HTTP Range requests.

    The keys sit at the front of the file and cost a few kilobytes. The
    tensors sit behind the tokenizer, so reading them costs the whole
    header, which for a large vocabulary is a few megabytes; they are only
    read when the model is a mixture of experts and the split between the
    shared part and the experts is needed.
    """

    CHUNK = 2 * 1024 * 1024
    ROUND = 4

    def __init__(self, url, token, timeout=60):
        self.url = url
        self.token = token
        self.timeout = timeout
        self.data = b""
        self.keys = {}
        self.tensors = []
        self.tensors_read = False

    def _pull(self, start, end):
        """Read one range and add it to the buffer."""
        chunk = fetch_range(self.url, self.token, start, end, "the GGUF header",
                            self.timeout)
        if not chunk:
            return False
        self.data += chunk
        return True

    def _round(self):
        """Read the next few megabytes, a few ranges at a time."""
        start = len(self.data)
        spans = [(start + self.CHUNK * index, start + self.CHUNK * (index + 1) - 1)
                 for index in range(self.ROUND)]
        with futures.ThreadPoolExecutor(max_workers=self.ROUND) as pool:
            parts = list(pool.map(lambda span: fetch_range(
                self.url, self.token, span[0], span[1], "the GGUF header", self.timeout),
                spans))
        for part in parts:
            if not part:
                break
            self.data += part
        return len(self.data) > start

    def read_keys(self):
        """Read the key/value pairs of the header."""
        self._pull(0, HEADER_FIRST_WINDOW - 1)
        arch = None
        while True:
            try:
                self.keys, _ = parse_gguf(self.data, False, {"general.architecture"})
                arch = self.keys.get("general.architecture")
                break
            except NeedMore:
                if not self._round():
                    raise FitError(ERROR, "the GGUF header ends in the middle") from None
        while True:
            try:
                self.keys, _ = parse_gguf(self.data, False, wanted_keys(arch))
                return self
            except NeedMore:
                if not self._round():
                    raise FitError(ERROR, "the GGUF header ends in the middle") from None

    def read_tensors(self):
        """Read the tensor list, which sits behind the whole header."""
        while True:
            try:
                self.keys, self.tensors = parse_gguf(self.data, True, None)
                self.tensors_read = True
                return self
            except NeedMore:
                if len(self.data) >= HEADER_LIMIT or not self._round():
                    return self



# ---------------------------------------------------------------- what the model is

class Numbers:
    """What the KV cache and the byte split need to know about a model."""

    def __init__(self, source):
        self.source = source
        self.architecture = ""
        self.layers = None
        self.kv_layers = None
        self.kv_heads = None
        self.kv_heads_per_layer = None
        self.head_dim = None
        self.value_dim = None
        self.experts = None
        self.experts_used = None
        self.max_context = None
        self.sliding_window = None
        self.sliding_layers = None
        self.cache_layers = None

    @property
    def mixture(self):
        return bool(self.experts and self.experts > 1 and self.experts_used)

    def as_json(self):
        return {"architecture": self.architecture, "layers": self.layers,
                "kv_layers": self.kv_layers, "kv_heads": self.kv_heads,
                "kv_heads_per_layer": list(self.kv_heads_per_layer or []),
                "head_dim": self.head_dim, "value_dim": self.value_dim,
                "experts": self.experts, "experts_used": self.experts_used,
                "max_context_length": self.max_context,
                "sliding_window": self.sliding_window,
                "sliding_layers": sum(1 for flag in self.sliding_layers or []
                                      if flag),
                "source": self.source}


def _as_int(value):
    if isinstance(value, bool):
        return None
    if isinstance(value, int):
        return value
    if isinstance(value, float) and value.is_integer():
        return int(value)
    return None


def numbers_from_gguf(keys):
    """The numbers of a model, from the header of its GGUF file."""
    arch = keys.get("general.architecture") or ""
    numbers = Numbers("gguf-header")
    numbers.architecture = str(arch)

    def value(name):
        return keys.get(f"{arch}.{name}")

    numbers.layers = _as_int(value("block_count"))
    numbers.max_context = _as_int(value("context_length"))
    heads = _as_int(value("attention.head_count"))
    kv_heads = _as_int(value("attention.head_count_kv"))
    numbers.kv_heads = kv_heads or heads
    key_length = _as_int(value("attention.key_length"))
    value_length = _as_int(value("attention.value_length"))
    numbers.head_dim = key_length or value_length
    numbers.value_dim = value_length or key_length
    numbers.experts = _as_int(value("expert_count"))
    numbers.experts_used = _as_int(value("expert_used_count"))
    numbers.cache_layers = _cache_layers(numbers.layers,
                                         value("attention.recurrent_layers"),
                                         _as_int(value("full_attention_interval")))
    numbers.kv_layers = (len(numbers.cache_layers)
                          if numbers.cache_layers is not None else None)
    # a model can keep a different number of heads, or a different window, in
    # each of its layers: the header says so, layer by layer
    numbers.kv_heads_per_layer = _int_list(value("attention.head_count_kv"),
                                            numbers.layers)
    numbers.sliding_layers = _bool_list(value("attention.sliding_window_pattern"),
                                        numbers.layers)
    numbers.sliding_window = (_as_int(value("attention.sliding_window"))
                              or numbers.sliding_window)
    return numbers


def _int_list(value, length):
    """A list of numbers of the header, when it holds one per layer."""
    if not isinstance(value, list) or length is None or len(value) != length:
        return None
    numbers = []
    for item in value:
        number = _as_int(item)
        if number is None:
            return None
        numbers.append(number)
    return numbers


def _bool_list(value, length):
    """A list of flags of the header, when it holds one per layer."""
    if not isinstance(value, list) or length is None or len(value) != length:
        return None
    return [bool(item) for item in value]


def _cache_layers(layers, recurrent, interval):
    """The layers that keep a KV cache.

    A hybrid model keeps a cache only in its full attention layers; the
    header says which ones those are, and how often they come otherwise.
    """
    if layers is None:
        return None
    if isinstance(recurrent, list) and len(recurrent) == layers:
        return [index for index, flag in enumerate(recurrent) if not flag]
    if interval and interval > 0:
        step = int(interval)
        keep = [index for index in range(layers) if (index + 1) % step == 0]
        if layers % step:
            keep.append(layers - 1)
        return keep
    return list(range(layers))


def numbers_from_config(config):
    """The numbers of a model, from the config.json of a transformers repo."""
    text = config.get("text_config")
    if not isinstance(text, dict):
        text = config
    numbers = Numbers("config.json")
    numbers.architecture = str(text.get("model_type") or config.get("model_type") or "")
    numbers.layers = _as_int(text.get("num_hidden_layers") or text.get("n_layer")
                              or text.get("num_layers"))
    heads = _as_int(text.get("num_attention_heads"))
    numbers.kv_heads = _as_int(text.get("num_key_value_heads")
                               or text.get("num_kv_heads")) or heads
    head_dim = _as_int(text.get("head_dim") or text.get("kv_channels"))
    if not head_dim and heads:
        hidden = _as_int(text.get("hidden_size"))
        if hidden:
            head_dim = hidden // heads
    numbers.head_dim = head_dim
    numbers.value_dim = head_dim
    numbers.experts = _as_int(text.get("num_experts") or text.get("num_local_experts")
                              or text.get("moe_num_experts") or text.get("n_routed_experts"))
    numbers.experts_used = _as_int(text.get("num_experts_per_tok")
                                   or text.get("num_experts_per_token")
                                   or text.get("moe_num_experts_per_tok")
                                   or text.get("top_k"))
    numbers.max_context = _as_int(text.get("max_position_embeddings")
                                  or text.get("max_seq_len"))
    numbers.sliding_window = _as_int(text.get("sliding_window"))
    numbers.kv_layers = _kv_layers_config(numbers.layers, text.get("layer_types"),
                                           _as_int(text.get("full_attention_interval")),
                                           text.get("sliding_window_pattern"))
    return numbers


def _kv_layers_config(layers, layer_types, interval, pattern):
    """How many layers keep a KV cache, in a transformers layout."""
    if isinstance(layer_types, list) and layer_types:
        keep = 0
        for kind in layer_types:
            name = str(kind)
            if name.endswith("attention") and not name.startswith("linear"):
                keep += 1
        if keep:
            return keep
    if layers is None:
        return None
    if pattern and pattern > 0:
        step = int(pattern)
        keep = sum(1 for index in range(layers) if index % step == 0)
        return keep
    if interval and interval > 0:
        step = int(interval)
        keep = sum(1 for index in range(layers) if (index + 1) % step == 0)
        if layers % step:
            keep += 1
        return keep
    return layers


# ---------------------------------------------------------------- the files

QUANT_PATTERN = re.compile(
    r"(IQ\d+(?:_[A-Z]+)*"          # the IQ family
    r"|Q\d+(?:_\d+)?(?:_K)?(?:_(?:S|M|L|XL))?"   # the Q family
    r"|BF16|F16|F32"
    r"|MXFP4(?:_[A-Z]+)*)")

# How many bits a weight takes, by quant label. It only orders the choices;
# the size of the file decides what fits.
QUANT_BITS = {
    "F32": 32.0, "BF16": 16.0, "F16": 16.0,
    "Q8_0": 8.5, "Q8_K": 8.5, "Q6_K": 6.56,
    "Q5_K": 5.5, "Q5_K_M": 5.69, "Q5_K_S": 5.5, "Q5_K_XL": 5.69, "Q5_0": 5.5, "Q5_1": 5.5,
    "Q4_K": 4.5, "Q4_K_M": 4.85, "Q4_K_S": 4.5, "Q4_K_XL": 4.85, "Q4_0": 4.5, "Q4_1": 4.5,
    "IQ4_XS": 4.25, "IQ4_NL": 4.5,
    "Q3_K": 3.9, "Q3_K_M": 3.9, "Q3_K_S": 3.9, "Q3_K_XL": 3.9,
    "IQ3_M": 3.68, "IQ3_S": 3.4, "IQ3_XXS": 3.06,
    "Q2_K": 2.63, "Q2_K_S": 2.63, "Q2_K_XL": 2.63,
    "IQ2_M": 2.83, "IQ2_S": 2.5, "IQ2_XS": 2.31, "IQ2_XXS": 2.06,
    "IQ1_M": 1.75, "IQ1_S": 1.54,
    "MXFP4": 4.0, "MXFP4_MOE": 4.0,
}

SPLIT_PATTERN = re.compile(r"-(\d{5})-of-(\d{5})$")


class File:
    """One file of a repository: its name and its size."""

    def __init__(self, name, size):
        self.name = name
        self.size = size

    @property
    def folder(self):
        return self.name.split("/")[0] if "/" in self.name else ""

    def as_json(self):
        return {"name": self.name, "bytes": self.size}


def quant_label(name):
    """The quant label of a file name, `Q4_K_M` for `model-Q4_K_M.gguf`."""
    stem = name.rsplit("/", 1)[-1]
    if stem.lower().endswith(".gguf"):
        stem = stem[:-5]
    match = QUANT_PATTERN.search(stem)
    return match.group(1) if match else None


def quant_bits(label):
    """How many bits a weight of this quant takes, when it is known."""
    if not label:
        return None
    if label in QUANT_BITS:
        return QUANT_BITS[label]
    base = re.sub(r"_(S|M|L|XL)$", "", label)
    return QUANT_BITS.get(base)


class Option:
    """One installable model: the files of a quant, split or not."""

    def __init__(self, label, files):
        self.label = label
        self.files = files

    @property
    def bytes(self):
        return sum(file.size or 0 for file in self.files)

    @property
    def bits(self):
        return quant_bits(self.label)

    def as_json(self):
        return {"quant": self.label, "files": [f.as_json() for f in self.files],
                "bytes": self.bytes, "quality_bits": self.bits}


def is_projector(name):
    stem = name.rsplit("/", 1)[-1].lower()
    return stem.startswith("mmproj")


def one_projector(files):
    """The projector a server loads: one file, not every precision the
    repository offers. F16 first (what runtimes load by default), then BF16,
    then the smallest of the rest."""
    if not files:
        return None
    for want in ("f16", "bf16"):
        for file in files:
            stem = file.name.rsplit("/", 1)[-1].lower()
            if stem.endswith(f"-{want}.gguf") or stem.endswith(f"_{want}.gguf"):
                return file
    return min(files, key=lambda file: file.size or 0)


def is_draft(name):
    stem = name.rsplit("/", 1)[-1].lower()
    return "mtp" in stem or "draft" in stem


def group_gguf(files):
    """The model files of a repository, one option per quant.

    A model too big for one file comes as `-00001-of-00003`; the parts
    belong together.
    """
    groups = {}
    for file in files:
        stem = file.name[:-5] if file.name.lower().endswith(".gguf") else file.name
        key = SPLIT_PATTERN.sub("", stem)
        groups.setdefault(key, []).append(file)
    options = []
    for key, parts in groups.items():
        label = None
        for part in sorted(parts, key=lambda f: f.name):
            label = quant_label(part.name)
            if label:
                break
        options.append(Option(label or "unknown", sorted(parts, key=lambda f: f.name)))
    return options


def rank_options(options):
    """The best option first: more bits per weight is a better model."""
    def key(option):
        bits = option.bits
        return (0 if bits is not None else 1, -(bits or 0.0), option.label or "")
    return sorted(options, key=key)


# ---------------------------------------------------------------- the machines

class Machine:
    """What a machine has: memory, and how fast its memory is."""

    def __init__(self, name, kind, budget, host, bandwidth, others=0, compute=None, min_free=None):
        self.name = name
        self.kind = kind
        self.budget = budget
        self.host = host
        self.bandwidth = bandwidth
        # Memory other users of the GPU hold at the card's normal state (a
        # desktop, an image server): never the model's.
        self.others = others or 0
        # The runtime's compute buffers (flash attention, the batch).
        self.compute = MARGIN_BYTES if compute is None else compute
        # The card memory a memory or context change must leave free
        # (`sb ai check`'s margin rule); None: its default.
        self.min_free = min_free

    @property
    def room(self):
        """What the model may take: the budget less the other users."""
        return max(0, self.budget - self.others)


def machine_from_entry(entry, where):
    """One machine of a machine file."""
    if not isinstance(entry, dict):
        raise FitError(USAGE, f"{where}: every machine is an object")
    name = str(entry.get("name") or "machine")
    kind = str(entry.get("kind") or "gpu").lower()
    if kind not in ("gpu", "unified"):
        raise FitError(USAGE, f"{where}: the machine {name} needs kind gpu or unified")
    if kind == "gpu":
        if entry.get("vram_gb") is None:
            raise FitError(USAGE, f"{where}: the machine {name} needs vram_gb")
        budget = float(entry["vram_gb"]) * GB
    else:
        if entry.get("usable_gb") is None:
            raise FitError(USAGE, f"{where}: the machine {name} needs usable_gb")
        budget = float(entry["usable_gb"]) * GB
    host = float(entry["ram_gb"]) * GB if entry.get("ram_gb") is not None else None
    bandwidth = float(entry["bandwidth_gbs"]) if entry.get("bandwidth_gbs") is not None else None
    others = float(entry["others_gb"]) * GB if entry.get("others_gb") is not None else 0
    compute = float(entry["compute_gb"]) * GB if entry.get("compute_gb") is not None else None
    min_free = float(entry["min_free_gb"]) * GB if entry.get("min_free_gb") is not None else None
    return Machine(name, kind, budget, host, bandwidth, others, compute, min_free)


def read_machines(path):
    """The machines of a machine file."""
    try:
        with open(path, encoding="utf-8") as handle:
            raw = json.load(handle)
    except FileNotFoundError:
        raise FitError(USAGE, f"the machine file {path} does not exist") from None
    except OSError as error:
        raise FitError(USAGE, f"cannot read the machine file {path}: {error.strerror}") from None
    except ValueError:
        raise FitError(USAGE, f"the machine file {path} is not JSON") from None
    entries = raw.get("machines") if isinstance(raw, dict) else raw
    if not isinstance(entries, list):
        raise FitError(USAGE, f"the machine file {path} needs a machines list")
    return [machine_from_entry(entry, f"the machine file {path}") for entry in entries]


def ad_hoc_machine(args):
    """The one machine the flags describe."""
    if args.vram is not None:
        budget = float(args.vram) * GB
        host = float(args.ram) * GB if args.ram is not None else None
        return Machine("ad-hoc", "gpu", budget, host, args.bandwidth, others_of(args),
                       compute_of(args))
    budget = float(args.ram) * GB
    return Machine("ad-hoc", "unified", budget, None, args.bandwidth, others_of(args),
                   compute_of(args))


def others_of(args):
    others = getattr(args, "others", None)
    return float(others) * GB if others is not None else 0


def compute_of(args):
    compute = getattr(args, "compute", None)
    return float(compute) * GB if compute is not None else None


def load_machines(args, environ):
    """The machines to judge against: a file, the flags, or none at all."""
    path = args.machines or environ.get("SB_AI_MACHINES")
    if path:
        machines = read_machines(path)
        for machine in machines:
            if getattr(args, "others", None) is not None:
                machine.others = others_of(args)
            if getattr(args, "compute", None) is not None:
                machine.compute = compute_of(args)
        return machines
    if args.vram is not None or args.ram is not None:
        return [ad_hoc_machine(args)]
    # No local machine file and no flags: the gateway's own machine facts
    # (`server.optimize.hosts`), when the caller fetched them.
    machines = []
    for entry in getattr(args, "gateway_machines", None) or []:
        try:
            machine = machine_from_entry(entry, "the gateway's machine facts")
        except FitError:
            continue
        if getattr(args, "others", None) is not None:
            machine.others = others_of(args)
        if getattr(args, "compute", None) is not None:
            machine.compute = compute_of(args)
        machines.append(machine)
    return machines


# ---------------------------------------------------------------- the fit

def kv_bytes_for(numbers, ctx, weights_bytes, cache_k="f16", cache_v="f16"):
    """The KV cache at a context length, and whether it is known.

    A layer can keep a different number of heads, or a shorter window, than
    the one above it, so the cache is added up layer by layer.

    When the header does not say how the model is put together, a tenth of
    the weights is reserved for the cache, so a model that does not fit is
    not called a model that fits.
    """
    k_bytes = CACHE_TYPE_BYTES.get(cache_k or "f16", CACHE_ELEMENT_BYTES)
    v_bytes = CACHE_TYPE_BYTES.get(cache_v or "f16", CACHE_ELEMENT_BYTES)
    width = (numbers.head_dim or 0) + (numbers.value_dim or numbers.head_dim or 0)
    # Bytes one token keeps per KV head: its key at the K type, its value
    # at the V type.
    per_head = ((numbers.head_dim or 0) * k_bytes
                + (numbers.value_dim or numbers.head_dim or 0) * v_bytes)
    if numbers.kv_layers and width and (numbers.kv_heads or numbers.kv_heads_per_layer):
        if numbers.cache_layers and (numbers.kv_heads_per_layer or numbers.sliding_layers):
            total = 0
            for index in numbers.cache_layers:
                heads = numbers.kv_heads
                if numbers.kv_heads_per_layer:
                    heads = numbers.kv_heads_per_layer[index]
                tokens = ctx
                if (numbers.sliding_layers and numbers.sliding_layers[index]
                        and numbers.sliding_window):
                    tokens = min(ctx, numbers.sliding_window)
                total += tokens * heads * per_head
            return int(total), True
        value = numbers.kv_layers * ctx * numbers.kv_heads * per_head
        return int(value), True
    return int(weights_bytes * UNKNOWN_KV_SHARE), False


def bytes_per_token(numbers, option_bytes, share):
    """How many bytes a token reads.

    A dense model reads every weight. A mixture of experts reads the shared
    part and the experts it picked, so only a share of the expert part.
    """
    if not numbers.mixture:
        return option_bytes
    if share is None:
        return None
    experts = option_bytes * share
    shared = option_bytes - experts
    return shared + (numbers.experts_used / numbers.experts) * experts


def speed_estimate(machine, option, per_token, on_gpu, on_host):
    """How fast the model would write, in tokens per second. An estimate."""
    if not machine.bandwidth or not per_token or not option.bytes:
        return None
    gpu_part = per_token * (on_gpu / option.bytes)
    host_part = per_token * (on_host / option.bytes)
    effective = gpu_part + host_part * OFFLOAD_PENALTY
    if effective <= 0:
        return None
    return machine.bandwidth * 1e9 * FACTOR / effective


class Analysis:
    """What one repository is, and what it needs."""

    def __init__(self, repo, kind, ctx, cache_k="f16", cache_v="f16"):
        self.repo = repo
        self.cache_k = cache_k or "f16"
        self.cache_v = cache_v or "f16"
        self.kind = kind
        self.ctx = ctx
        self.numbers = Numbers("unknown")
        self.options = []
        self.projector = []
        self.draft = []
        self.components = []
        self.expert_share = None
        self.notes = []
        self.pipeline = ""

    def as_json(self):
        biggest = max((option.bytes for option in self.options), default=0)
        kv, known = kv_bytes_for(self.numbers, self.ctx, biggest, self.cache_k, self.cache_v)
        options = []
        for option in self.options:
            row = option.as_json()
            per_token = bytes_per_token(self.numbers, option.bytes, self.expert_share)
            row["bytes_per_token"] = per_token
            row["bytes_per_token_source"] = (
                "gguf-header" if self.expert_share is not None else
                ("file size" if not self.numbers.mixture else "unknown"))
            options.append(row)
        return {
            "repo": self.repo,
            "kind": self.kind,
            "pipeline_tag": self.pipeline,
            "context_length": self.ctx,
            "model": self.numbers.as_json(),
            "kv_cache": {"bytes": kv, "known": known,
                         "element_bytes": CACHE_TYPE_BYTES.get(self.cache_k, CACHE_ELEMENT_BYTES),
                         "cache_type_k": self.cache_k, "cache_type_v": self.cache_v,
                         "note": ("the KV cache is unknown; a tenth of the weights "
                                  "is reserved for it" if not known else
                                  f"a {self.cache_k}/{self.cache_v} KV cache at the given "
                                  "context length")},
            "options": options,
            "projector": {"files": [f.as_json() for f in self.projector],
                          "chosen": (one_projector(self.projector).name
                                     if self.projector else None),
                          "bytes": (one_projector(self.projector).size or 0
                                    if self.projector else 0)},
            "draft": {"files": [f.as_json() for f in self.draft],
                      "bytes": sum(f.size or 0 for f in self.draft)},
            "components": self.components,
            "machines": [],
            "notes": list(self.notes),
        }


def classify(data, files):
    """What kind of repository this is.

    A diffusion repository keeps its parts in folders: the model itself, the
    text encoders, the loras. A GGUF repository is a text model, whatever
    its pipeline tag says: the header of the file says which architecture it
    is, and a language model is a language model even when it also looks at
    pictures. The pipeline tag only speaks for a repository that holds no
    GGUF file.
    """
    folders = {file.folder for file in files if file.folder}
    if any(folder in COMPONENT_FOLDERS for folder in folders):
        return "diffusion"
    names = [file.name.lower() for file in files]
    if any(name.endswith(".gguf") for name in names):
        return "gguf"
    pipeline = (data.get("pipeline_tag") or "").lower()
    if pipeline and pipeline not in TEXT_TAGS:
        return "diffusion"
    if any(name.endswith(".safetensors") for name in names):
        return "safetensors"
    return "unknown"


def gguf_options(files):
    """The model files of a repository, one option per quant.

    The projector and the draft head go with the model, but they are not
    the model, so they are kept apart.
    """
    gguf = [file for file in files if file.name.lower().endswith(".gguf")]
    projector = [file for file in gguf if is_projector(file.name)]
    draft = [file for file in gguf if is_draft(file.name)]
    weights = [file for file in gguf
               if file not in projector and file not in draft]
    return rank_options(group_gguf(weights)), projector, draft


def fill_gguf(analysis, repo, files, token):
    """A GGUF repository: the quants, and the header of one of them."""
    analysis.options, analysis.projector, analysis.draft = gguf_options(files)
    if not analysis.options:
        analysis.notes.append("the repository has no GGUF model file")
        return
    first = analysis.options[0].files[0]
    url = f"{HF_BASE}/{repo}/resolve/main/{urllib.parse.quote(first.name)}"
    header = Header(url, token).read_keys()
    analysis.numbers = numbers_from_gguf(header.keys)
    if analysis.numbers.architecture in DIFFUSION_ARCHITECTURES:
        # the header says the file holds a picture and not words, so the
        # repository is a diffusion repository after all
        analysis.kind = "diffusion"
        analysis.options = []
        analysis.projector = []
        analysis.draft = []
        fill_diffusion(analysis, files)
        return
    if analysis.numbers.mixture:
        # the split between the shared part and the experts needs the tensors
        header.read_tensors()
        analysis.expert_share = expert_share(header.tensors)
        if analysis.expert_share is None:
            analysis.notes.append("the experts could not be told apart; the speed "
                                  "estimate is unknown")


def fill_safetensors(analysis, repo, files, token):
    """A transformers repository: one model, and the total size."""
    shards = [file for file in files if file.name.lower().endswith(".safetensors")]
    analysis.options = [Option("safetensors", sorted(shards, key=lambda f: f.name))]
    config = fetch_bytes(f"{HF_BASE}/{repo}/resolve/main/config.json", token,
                         "the config of the model")
    if config:
        try:
            analysis.numbers = numbers_from_config(json.loads(config.decode("utf-8")))
        except (ValueError, UnicodeDecodeError):
            analysis.notes.append("the config of the model is not JSON")


def fill_diffusion(analysis, files):
    """A diffusion repository: the parts, by folder."""
    for file in files:
        if not file.name.lower().endswith(MODEL_SUFFIXES):
            continue
        folder = file.folder
        role = "component" if folder in AUX_FOLDERS else "model"
        analysis.components.append({"folder": folder, "name": file.name,
                                    "bytes": file.size, "role": role})
    if not analysis.components:
        analysis.notes.append("the repository has no model file")


def no_fit_reason(machine, analysis, projector_bytes):
    """Why nothing fits, in one line."""
    biggest = max((option.bytes for option in analysis.options), default=0)
    kv, _ = kv_bytes_for(analysis.numbers, analysis.ctx, biggest, analysis.cache_k,
                         analysis.cache_v)
    if kv + projector_bytes + machine.compute > machine.room:
        return ("the KV cache and the compute margin do not fit in the memory "
                "of this machine")
    if machine.kind == "gpu" and machine.host is None:
        return ("the model does not fit, and the host memory is not known, so "
                "the rest cannot be offloaded")
    if machine.kind == "gpu":
        return ("the model does not fit, and the host memory is too small for "
                "the rest")
    return ("the model does not fit in the unified memory, and unified memory "
            "has no second place to offload to")


def judge_machine(machine, analysis):
    """What this machine should take, and what it would cost."""
    row = {"name": machine.name, "kind": machine.kind,
           "budget_bytes": machine.budget, "host_bytes": machine.host,
           "others_bytes": machine.others, "compute_bytes": machine.compute,
           "bandwidth_gbs": machine.bandwidth,
           "verdict": "no-fit", "warnings": []}
    if analysis.kind == "diffusion":
        return judge_diffusion(machine, analysis, row)
    chosen = one_projector(analysis.projector)
    projector_bytes = (chosen.size or 0) if chosen else 0
    choice = None
    for option in analysis.options:
        kv, _ = kv_bytes_for(analysis.numbers, analysis.ctx, option.bytes,
                              analysis.cache_k, analysis.cache_v)
        needed = option.bytes + kv + projector_bytes + machine.compute
        if needed <= machine.room:
            choice = {"option": option, "kv": kv, "needed": needed,
                      "on_gpu": option.bytes, "on_host": 0}
            break
    if choice is None:
        # Nothing fits whole. The best that can be partly offloaded wins, and
        # the one that writes fastest is worth naming as well.
        offered = []
        for option in analysis.options:
            kv, _ = kv_bytes_for(analysis.numbers, analysis.ctx, option.bytes,
                              analysis.cache_k, analysis.cache_v)
            room = machine.room - kv - projector_bytes - machine.compute
            if room <= 0 or machine.kind != "gpu" or machine.host is None:
                continue
            on_gpu = min(option.bytes, room)
            on_host = option.bytes - on_gpu
            if on_host > machine.host:
                continue
            per_token = bytes_per_token(analysis.numbers, option.bytes,
                                        analysis.expert_share)
            offered.append({"option": option, "kv": kv, "needed": machine.room,
                            "on_gpu": on_gpu, "on_host": on_host,
                            "per_token": per_token,
                            "speed": speed_estimate(machine, option, per_token,
                                                    on_gpu, on_host)})
        if offered:
            choice = offered[0]
            fastest = max(offered, key=lambda item: item["speed"] or 0)
            if fastest is not choice and (fastest["speed"] or 0) > 1.5 * (choice["speed"] or 0):
                choice["faster"] = fastest
    if choice is None:
        row["warnings"].append(no_fit_reason(machine, analysis, projector_bytes))
        return row
    option = choice["option"]
    row["verdict"] = "fits" if not choice["on_host"] else "partial"
    row["quant"] = option.label
    row["files"] = [f.as_json() for f in option.files]
    row["weights_bytes"] = option.bytes
    row["kv_cache_bytes"] = choice["kv"]
    row["projector_bytes"] = projector_bytes
    row["margin_bytes"] = machine.compute
    row["needed_bytes"] = choice["needed"]
    row["on_gpu_bytes"] = choice["on_gpu"]
    row["offloaded_bytes"] = choice["on_host"]
    row["offloaded_fraction"] = (choice["on_host"] / option.bytes) if option.bytes else 0.0
    per_token = bytes_per_token(analysis.numbers, option.bytes, analysis.expert_share)
    row["bytes_per_token"] = per_token
    row["speed_estimate_tok_s"] = speed_estimate(machine, option, per_token,
                                                  choice["on_gpu"], choice["on_host"])
    row["disk_bytes"] = option.bytes + projector_bytes
    row["faster_alternative"] = None
    if choice["on_host"]:
        row["warnings"].append("the weights do not all fit in the GPU: the rest "
                               "comes from host memory, and the speed falls by "
                               "about an order of magnitude")
        faster = choice.get("faster")
        if faster:
            row["faster_alternative"] = {
                "quant": faster["option"].label,
                "files": [f.as_json() for f in faster["option"].files],
                "bytes": faster["option"].bytes,
                "speed_estimate_tok_s": faster["speed"]}
            row["warnings"].append(
                f"the smaller {faster['option'].label} would write about "
                f"{faster['speed']:.0f} tok/s instead")
    return row


def judge_diffusion(machine, analysis, row):
    """Which parts of a diffusion model fit, and which have to be offloaded."""
    models = [part for part in analysis.components
              if part["role"] == "model" and part["bytes"]]
    parts = [part for part in analysis.components
             if part["role"] == "component" and part["bytes"]]
    best = None
    for part in sorted(models, key=lambda p: (-p["bytes"], p["name"])):
        if part["bytes"] + machine.compute <= machine.room:
            best = part
            break
    if best is None:
        smallest = min((part["bytes"] for part in models), default=0)
        row["warnings"].append(f"the smallest model file is {human_bytes(smallest)} "
                               "and does not fit in the memory of this machine")
        return row
    row["verdict"] = "fits"
    row["model"] = best
    room = machine.room - best["bytes"] - machine.compute
    on_gpu = []
    offload = []
    for part in sorted(parts, key=lambda p: (COMPONENT_ORDER.get(p["folder"], 4),
                                             p["name"])):
        if part["bytes"] <= room:
            on_gpu.append(part)
            room -= part["bytes"]
        else:
            offload.append(part)
    row["on_gpu"] = on_gpu
    row["offload"] = offload
    row["disk_bytes"] = best["bytes"] + sum(part["bytes"] for part in on_gpu)
    if offload:
        row["warnings"].append("some parts do not fit in memory and have to be "
                               "offloaded to the host")
    return row


# ---------------------------------------------------------------- the answer

def human_bytes(count):
    """A size for people."""
    if count is None:
        return "unknown"
    if count >= 1e9:
        return f"{count / 1e9:.2f} GB"
    if count >= 1e6:
        return f"{count / 1e6:.2f} MB"
    if count >= 1e3:
        return f"{count / 1e3:.1f} kB"
    return f"{count} B"


def model_words(model):
    """One line about the model itself."""
    words = []
    if model.get("layers"):
        words.append(f"{model['layers']} layers")
    if model.get("kv_layers") and model.get("layers") and \
            model["kv_layers"] != model["layers"]:
        words.append(f"{model['kv_layers']} of them keep a KV cache")
    if model.get("experts"):
        used = model.get("experts_used")
        words.append(f"{model['experts']} experts"
                     + (f" ({used} used per token)" if used else ""))
    return ": " + ", ".join(words) if words else ""


def render(report):
    """The answer for people."""
    lines = [f"sb ai fit: {report['repo']}"]
    kind = report["kind"]
    model = report["model"]
    if kind == "gguf":
        lines.append("  a text model in gguf" + model_words(model))
    elif kind == "safetensors":
        lines.append("  a text model in safetensors" + model_words(model))
    elif kind == "diffusion":
        tag = report.get("pipeline_tag") or "no pipeline tag"
        lines.append(f"  a diffusion model ({tag})")
    else:
        lines.append("  nothing here can be measured")
    if kind == "diffusion":
        lines.extend(render_components(report))
    else:
        lines.extend(render_files(report))
        lines.extend(render_cache(report))
    for machine in report["machines"]:
        lines.extend(render_machine(machine, report))
    for note in report["notes"]:
        lines.append(f"  note: {note}")
    return "\n".join(lines) + "\n"


def render_files(report):
    """The files of a text repository, and the projector."""
    rows = []
    for option in report["options"]:
        for file in option["files"]:
            rows.append((option["quant"] or "unknown", file["bytes"], file["name"]))
    projector = report["projector"]["files"]
    for file in projector:
        rows.append(("projector", file["bytes"], file["name"]))
    draft = report["draft"]["files"]
    for file in draft:
        rows.append(("draft", file["bytes"], file["name"]))
    if not rows:
        return ["  no model file"]
    width = max(len(row[0]) for row in rows)
    lines = ["  files"]
    for label, size, name in rows:
        lines.append(f"    {label.ljust(width)}  {human_bytes(size).rjust(9)}  {name}")
    return lines


def render_components(report):
    """The parts of a diffusion repository, by folder."""
    rows = [(part["folder"] or "(root)", part["bytes"], part["name"])
            for part in report["components"]]
    if not rows:
        return ["  no model file"]
    width = max(len(row[0]) for row in rows)
    lines = ["  components"]
    for folder, size, name in rows:
        lines.append(f"    {folder.ljust(width)}  {human_bytes(size).rjust(9)}  {name}")
    return lines


def render_cache(report):
    """The KV cache, and where its numbers come from."""
    cache = report["kv_cache"]
    kinds = f"{cache.get('cache_type_k', 'f16')}/{cache.get('cache_type_v', 'f16')}"
    lines = [f"  kv cache at {report['context_length']} tokens ({kinds}): "
             f"{human_bytes(cache['bytes'])}"]
    model = report["model"]
    words = []
    if model.get("kv_layers") and model.get("layers"):
        words.append(f"{model['kv_layers']} of {model['layers']} layers keep a cache")
    heads = model.get("kv_heads_per_layer") or []
    if heads:
        low, high = min(heads), max(heads)
        words.append(f"{low} to {high} kv heads" if low != high
                     else f"{low} kv heads")
    elif model.get("kv_heads"):
        words.append(f"{model['kv_heads']} kv heads")
    if model.get("head_dim"):
        words.append(f"{model['head_dim']} wide")
    if words:
        lines.append("    " + ", ".join(words))
    sliding = model.get("sliding_window")
    if sliding and model.get("sliding_layers"):
        lines.append(f"    {model['sliding_layers']} of them keep a sliding "
                     f"window of {sliding} tokens")
    if not cache["known"]:
        lines.append("    " + cache["note"])
    return lines


def render_machine(machine, report):
    """What one machine should take."""
    kind = machine["kind"]
    bits = [machine["name"], kind]
    if kind == "gpu":
        bits.append(f"{machine['budget_bytes'] / GB:.0f} GB vram")
        if machine.get("host_bytes"):
            bits.append(f"{machine['host_bytes'] / GB:.0f} GB ram")
    else:
        bits.append(f"{machine['budget_bytes'] / GB:.0f} GB usable")
    if machine.get("bandwidth_gbs"):
        bits.append(f"{machine['bandwidth_gbs']:.0f} GB/s")
    lines = ["  " + ", ".join(bits)]
    verdict = machine["verdict"]
    if machine.get("model"):
        # a diffusion repository: the model and the parts around it
        model = machine["model"]
        lines.append(f"    fits {model['name']} ({human_bytes(model['bytes'])})")
        for part in machine.get("on_gpu") or []:
            lines.append(f"      in memory: {part['name']} ({human_bytes(part['bytes'])})")
        for part in machine.get("offload") or []:
            lines.append(f"      offload:   {part['name']} ({human_bytes(part['bytes'])})")
    elif verdict == "fits":
        pieces = [f"{human_bytes(machine['weights_bytes'])} weights",
                  f"{human_bytes(machine['kv_cache_bytes'])} kv cache"]
        if machine.get("projector_bytes"):
            pieces.append(f"{human_bytes(machine['projector_bytes'])} projector")
        pieces.append(f"{human_bytes(machine['margin_bytes'])} compute")
        room = machine["budget_bytes"] - (machine.get("others_bytes") or 0)
        of = human_bytes(room)
        if machine.get("others_bytes"):
            of += (f" (of {human_bytes(machine['budget_bytes'])}, other GPU users hold "
                   f"{human_bytes(machine['others_bytes'])})")
        lines.append(f"    fits {machine.get('quant')}: " + " + ".join(pieces) +
                     f" = {human_bytes(machine['needed_bytes'])} of {of}")
    elif verdict == "partial":
        lines.append(f"    partial {machine.get('quant')}: "
                     f"{human_bytes(machine['on_gpu_bytes'])} of "
                     f"{human_bytes(machine['weights_bytes'])} in the GPU, "
                     f"{human_bytes(machine['offloaded_bytes'])} from host memory")
    else:
        lines.append("    no fit")
    details = []
    if machine.get("speed_estimate_tok_s"):
        details.append(f"about {machine['speed_estimate_tok_s']:.0f} tok/s (estimate)")
    if machine.get("disk_bytes"):
        details.append(f"disk {human_bytes(machine['disk_bytes'])}")
    if details:
        lines.append("      " + ", ".join(details))
    for warning in machine.get("warnings") or []:
        lines.append(f"      ! {warning}")
    return lines


# ---------------------------------------------------------------- the command

def build_report(args, environ):
    """Everything the command knows about a repository."""
    repo = repo_id(args.repo)
    token = read_token(environ)
    listing = f"{HF_BASE}/api/models/{urllib.parse.quote(repo)}?blobs=true"
    data = fetch_json(listing, token, "the repository listing")
    if not isinstance(data, dict):
        raise FitError(ERROR, "the repository listing is not readable")
    files = []
    for sibling in data.get("siblings") or []:
        name = sibling.get("rfilename")
        if not name:
            continue
        size = sibling.get("size")
        files.append(File(name, size if isinstance(size, int) else None))
    kind = classify(data, files)
    analysis = Analysis(repo, kind, args.ctx, getattr(args, "cache_type_k", None),
                        getattr(args, "cache_type_v", None))
    analysis.pipeline = data.get("pipeline_tag") or ""
    if kind == "gguf":
        fill_gguf(analysis, repo, files, token)
    elif kind == "safetensors":
        fill_safetensors(analysis, repo, files, token)
    elif kind == "diffusion":
        fill_diffusion(analysis, files)
    else:
        analysis.notes.append("this repository holds no model file that can be measured")
    if analysis.numbers.max_context and args.ctx > analysis.numbers.max_context:
        analysis.notes.append(f"the model takes {args.ctx} tokens of context here, "
                              f"but it was made for {analysis.numbers.max_context}")
    report = analysis.as_json()
    for machine in load_machines(args, environ):
        report["machines"].append(judge_machine(machine, analysis))
    return report


def add_arguments(parser):
    """The flags of `sb ai fit`, for the caller to add to a parser."""
    parser.add_argument("repo")
    parser.add_argument("--ctx", type=int, default=DEFAULT_CTX)
    parser.add_argument("--machines")
    parser.add_argument("--vram", type=float)
    parser.add_argument("--ram", type=float)
    parser.add_argument("--bandwidth", type=float)
    parser.add_argument("--json", action="store_true")
    parser.add_argument("--cache-type-k", choices=sorted(CACHE_TYPE_BYTES))
    parser.add_argument("--cache-type-v", choices=sorted(CACHE_TYPE_BYTES))
    parser.add_argument("--others", type=float)
    parser.add_argument("--compute", type=float)
    parser.add_argument("--target")


def parser():
    top = argparse.ArgumentParser(prog="sb ai fit", add_help=False, allow_abbrev=False)
    add_arguments(top)
    return top


def run(args, environ=None):
    """Answer the question, from arguments that are already parsed."""
    environ = os.environ if environ is None else environ
    if args.ctx < 1:
        print("sb ai: --ctx needs a context length in tokens", file=sys.stderr)
        return USAGE
    try:
        report = build_report(args, environ)
    except FitError as error:
        print(f"sb ai: {error.message}", file=sys.stderr)
        return error.code
    if args.json:
        print(json.dumps(report, indent=2))
    else:
        print(render(report), end="")
    return OK


def main(argv=None, environ=None):
    """Run `sb ai fit`."""
    return run(parser().parse_args(argv), environ)



