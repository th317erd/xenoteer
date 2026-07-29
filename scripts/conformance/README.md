# Conformance tools

The validator and runner are dependency-free Python 3 tools licensed under
Apache-2.0. The license and attribution notice are in
[`../../conformance/`](../../conformance/).

Validate the checked-in byte hashes, declarative semantics, coverage, and case
IDs:

```sh
python3 scripts/conformance/validate.py
```

List or filter stable case IDs:

```sh
python3 scripts/conformance/run.py --list
python3 scripts/conformance/run.py --operation decode_uint64_string --list
```

An SDK adapter is an executable that reads one JSON document from standard
input and writes one JSON document to standard output. The request contains
`adapter_protocol`, the corpus identity/hash, protocol range, and selected
cases. The response shape is:

```json
{
  "adapter_protocol": 1,
  "results": [
    {
      "detail": "",
      "id": "uint64.max",
      "status": "passed"
    }
  ]
}
```

The runner rejects missing, duplicate, and extra results. A skipped case fails
unless the caller explicitly supplies `--allow-skips`; release qualification
must not use that option.

```sh
python3 scripts/conformance/run.py --adapter ./path/to/sdk-adapter
```

Place all runner filters before `--adapter`; every remaining argument belongs
to the adapter command.
