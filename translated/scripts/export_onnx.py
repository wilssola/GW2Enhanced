"""Writes the two ONNX graphs of the DirectML mode, without any weights of their
own: every weight points into the `model.bin` the CUDA mode already uses, so the
same download serves both.

    python export_onnx.py <Hugging Face folder> <CTranslate2 model folder>

Goes next to model.bin:
encoder.onnx  (input_ids, attention_mask) -> encoder_hidden_states
decoder.onnx  (input_ids, encoder_hidden_states, encoder_attention_mask)
              -> logits of the LAST position only (the search needs nothing else)

The graphs compute in float16, like the CUDA mode; inputs and outputs are float32.
Token ids are those of shared_vocabulary.json, plus one more at the end: the
padding token, which CTranslate2 leaves out and which starts every translation.

Graphs are tied to the exact model.bin they were written for (they hold byte
offsets into it): run this again if the model is converted again.
"""

import sys
from pathlib import Path

import numpy as np
import onnx
import torch
from onnx import TensorProto, helper, numpy_helper
from transformers import MarianMTModel

OPSET = 17
# Below this, a tensor stays in the graph (scalars, shape constants).
MIN_EXTERNAL_BYTES = 4096


class Encoder(torch.nn.Module):
    def __init__(self, model):
        super().__init__()
        self.encoder = model.get_encoder()

    def forward(self, input_ids, attention_mask):
        out = self.encoder(input_ids=input_ids, attention_mask=attention_mask, return_dict=True)
        return out.last_hidden_state.float()


class Decoder(torch.nn.Module):
    def __init__(self, model):
        super().__init__()
        self.decoder = model.get_decoder()
        self.lm_head = model.lm_head
        self.register_buffer("bias", model.final_logits_bias)

    def forward(self, input_ids, encoder_hidden_states, encoder_attention_mask):
        hidden = self.decoder(
            input_ids=input_ids,
            encoder_hidden_states=encoder_hidden_states.half(),
            encoder_attention_mask=encoder_attention_mask,
            use_cache=False,
            return_dict=True,
        ).last_hidden_state
        return (self.lm_head(hidden[:, -1, :]) + self.bias).float()


def external(tensor_name, array, offset):
    """An initializer whose bytes live in model.bin."""
    t = TensorProto(name=tensor_name, data_type=TensorProto.FLOAT16, dims=array.shape)
    t.data_location = TensorProto.EXTERNAL
    for key, value in (("location", "model.bin"), ("offset", offset), ("length", array.nbytes)):
        entry = t.external_data.add()
        entry.key, entry.value = key, str(value)
    return t


def link_weights(path, weights):
    """Replaces the graph's weights with references into model.bin.

    A weight is found in model.bin by its bytes, in one of these forms:
    - as is;
    - transposed: the exporter folds the Transpose of each linear layer's
      kernel into the weight, so it is put back as a node;
    - without its last row or column (zero or unused): the padding token, which
      model.bin leaves out, is put back as a Concat with zeros.
    """
    model = onnx.load(str(path))
    graph = model.graph
    keep, nodes, linked, inline = [], [], 0, 0

    def find(array):
        offset = weights.find(array.tobytes())
        return None if offset < 0 else offset

    for init in graph.initializer:
        a = numpy_helper.to_array(init)
        if a.dtype != np.float16 or a.nbytes < MIN_EXTERNAL_BYTES:
            keep.append(init)
            continue
        name = init.name
        offset = find(a)
        if offset is not None:
            keep.append(external(name, a, offset))
            linked += a.nbytes
            continue
        if a.ndim == 2 and (offset := find(a.T)) is not None:
            keep.append(external(name + ".raw", a.T, offset))
            nodes.append(helper.make_node("Transpose", [name + ".raw"], [name], perm=[1, 0]))
            linked += a.nbytes
            continue
        if a.ndim == 2 and a.shape[0] > a.shape[1] and (offset := find(a[:-1])) is not None:
            # Embeddings: one row short in model.bin.
            head = a[:-1]
            keep.append(external(name + ".head", head, offset))
            keep.append(numpy_helper.from_array(a[-1:], name + ".tail"))
            nodes.append(helper.make_node("Concat", [name + ".head", name + ".tail"], [name], axis=0))
            linked += head.nbytes
            continue
        if a.ndim == 2 and a.shape[1] > a.shape[0] and (offset := find(a[:, :-1].T)) is not None:
            # Output projection: the same embeddings, transposed.
            head = a[:, :-1].T
            keep.append(external(name + ".head", head, offset))
            keep.append(numpy_helper.from_array(a[:, -1:], name + ".tail"))
            nodes.append(helper.make_node("Transpose", [name + ".head"], [name + ".flipped"], perm=[1, 0]))
            nodes.append(helper.make_node("Concat", [name + ".flipped", name + ".tail"], [name], axis=1))
            linked += head.nbytes
            continue
        print(f"  not in model.bin, kept in the graph: {name} {a.shape}")
        keep.append(init)
        inline += a.nbytes

    del graph.initializer[:]
    graph.initializer.extend(keep)
    # Nodes must come in topological order: these only depend on initializers.
    rest = list(graph.node)
    del graph.node[:]
    graph.node.extend(nodes + rest)
    onnx.save(model, str(path))
    print(f"{path.name}: {path.stat().st_size / 2**20:.2f} MB, {linked / 2**20:.0f} MB linked, {inline / 2**20:.1f} MB inline")
    return inline


def export(module, args, names, axes, output, path):
    torch.onnx.export(
        module,
        args,
        str(path),
        input_names=names,
        output_names=[output],
        dynamic_axes=axes,
        opset_version=OPSET,
        do_constant_folding=True,
        dynamo=False,
    )


def main():
    source, target = Path(sys.argv[1]), Path(sys.argv[2])
    weights = (target / "model.bin").read_bytes()
    model = MarianMTModel.from_pretrained(source, attn_implementation="eager").eval().half()
    config = model.config

    ids = torch.tensor([[10, 11, 12, config.eos_token_id], [10, 11, config.eos_token_id, config.pad_token_id]])
    mask = (ids != config.pad_token_id).long()
    hidden = Encoder(model)(ids, mask)
    encoder = target / "encoder.onnx"
    export(
        Encoder(model),
        (ids, mask),
        ["input_ids", "attention_mask"],
        {"input_ids": {0: "batch", 1: "source"}, "attention_mask": {0: "batch", 1: "source"}, "encoder_hidden_states": {0: "batch", 1: "source"}},
        "encoder_hidden_states",
        encoder,
    )
    start = torch.tensor([[config.decoder_start_token_id, 10, 11]] * 2)
    decoder = target / "decoder.onnx"
    export(
        Decoder(model),
        (start, hidden, mask),
        ["input_ids", "encoder_hidden_states", "encoder_attention_mask"],
        {
            "input_ids": {0: "batch", 1: "target"},
            "encoder_hidden_states": {0: "batch", 1: "source"},
            "encoder_attention_mask": {0: "batch", 1: "source"},
            "logits": {0: "batch"},
        },
        "logits",
        decoder,
    )
    inline = link_weights(encoder, weights) + link_weights(decoder, weights)
    if inline > 1 << 20:
        sys.exit(f"{inline / 2**20:.0f} MB of weights were not found in model.bin: is it the same model?")


if __name__ == "__main__":
    with torch.no_grad():
        main()
