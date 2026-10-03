# Third-party notices

## zeron

`tools/fixtures/resource-stream.jsonl` is copied from
[zeronsh/zeron](https://github.com/zeronsh/zeron) (`scripts/fixtures/resource-stream.jsonl`,
commit `9e1a11158b0626237c814f4bd36f5948483ed797`) so Blongo and zeron can be
profiled with the identical workload. Parts of `crates/blongo-harness` are
adapted from zeron's `crates/harness` where noted in the source.

```
MIT License

Copyright (c) 2026 Wing

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```

## t3code

Blongo's design follows [pingdotgg/t3code](https://github.com/pingdotgg/t3code)
(MIT, Copyright (c) 2026 T3 Tools Inc.). Where code is adapted it is noted in
the source.

`crates/blongo-harness/tests/fixtures/t3code/*.ndjson` are t3code's recorded
`codex app-server` sessions
(`apps/server/src/orchestration-v2/testkit/fixtures/<scenario>/codex_transcript.ndjson`,
commit `8ed276c246b624631e7d39241ebfd22d8314cb68`), used unchanged except that
model names and one git branch name in the recorded metadata were replaced by
neutral placeholders.

```
MIT License

Copyright (c) 2026 T3 Tools Inc.

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```

## GPUI

Blongo links [GPUI](https://github.com/zed-industries/zed/tree/main/crates/gpui)
(Apache-2.0, Copyright Zed Industries, Inc.) at the revision pinned in
`Cargo.toml`. `apps/blongo/src/input.rs` is adapted from GPUI's
`crates/gpui/examples/input.rs` (Apache-2.0), extended to multiple lines,
soft wrapping and scrolling. The Apache License 2.0 text (copied from GPUI's
repository) is in `licenses/Apache-2.0.txt`. Only Apache-2.0 GPUI crates are
used; none of Zed's GPL-licensed crates are depended on or copied.
