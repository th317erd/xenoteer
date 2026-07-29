# Xenoteer conformance corpus

This directory is the language-neutral Xenoteer protocol conformance boundary.
It is licensed under the Apache License, Version 2.0; the full terms are in
[`LICENSE`](LICENSE) and attribution is in [`NOTICE`](NOTICE). The separately
implemented Xenoteer server remains governed by the repository-root license.

## Version 1 layout

`v1/manifest.json` pins the supported protocol range, every suite's exact byte
hash and case count, plus a deterministic aggregate hash. Case files are
declarative JSON and make no language-specific assumptions:

- strict request and additive response/event compatibility;
- highest-common-minor negotiation and exact request version fencing;
- canonical `uint64-string` precision boundaries;
- command reconnect, deduplication, cancellation and unknown outcomes;
- generation- and birth-fenced stale references;
- effect stage and cleanup classification;
- event replay, resynchronization, filtering and backpressure;
- raw secret-bearing inputs checked against actual SDK debug, error, and URL
  surfaces.

Scenario cases carry concrete wire objects, transport faults, queue inputs,
and expected machine observations. They never ask adapters to echo named
actions or assertions; mutation tests prove that changing a command envelope
or event frame changes the observed result.

The case format itself is versioned independently through `format_version`.
Within one format version, case IDs and operation names are stable. Additive
case fields require a format-version increase because runners consume the
declarative objects directly.

Validate the complete corpus before invoking any SDK adapter:

```sh
python3 scripts/conformance/validate.py
```

The deterministic aggregate is:

```text
SHA256(path UTF-8 || NUL || lowercase file SHA256 ASCII || LF ...)
```

with suite paths in ascending byte order. The manifest is intentionally not
included in its own aggregate.

See [`../scripts/conformance/README.md`](../scripts/conformance/README.md) for
the dependency-free adapter runner contract.
