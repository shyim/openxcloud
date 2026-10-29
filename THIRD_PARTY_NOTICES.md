# Third-party notices

OpenXCloud is licensed under GPL-3.0-only. The full licence is in
[LICENSE](LICENSE). Third-party components retain their respective licences
and notices, as described below.

## AMD FidelityFX Super Resolution 1.0

The EASU and RCAS WGSL shaders in `crates/xcloud-gpu/src/shaders/` are ports of
AMD's `ffx_fsr1.h`. Both source files retain this notice:

```text
Copyright (c) 2021 Advanced Micro Devices, Inc. All rights reserved.

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in
all copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN
THE SOFTWARE.
```

## Rust and native dependencies

Rust dependency versions are recorded in `Cargo.lock`; package licence metadata is
available through `cargo metadata --locked --format-version 1`. Dependencies retain
their own licences. FFmpeg and Opus are native media dependencies; the exact FFmpeg
build and its enabled components must be recorded for each distributed package.
System frameworks and system fonts are loaded from the operating system.

This source attribution file is not a complete binary dependency licence bundle.
Generate and review that bundle against the exact release artifacts before shipping,
including native runtime libraries and transitive Rust dependency notices.
