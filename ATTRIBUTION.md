# Attribution

This project is an independent reimplementation, but part of what ships in it is
derived from another project and carries that project's licence.

## @zereight/mcp-gitlab

Upstream: https://github.com/zereight/gitlab-mcp — MIT licence.

`data/tools.json` holds the 262 tool names, human-readable descriptions and JSON
input schemas extracted from that package (version 2.1.60). The descriptions are
its authors' text, reproduced so that a client sees the same interface. Roughly
130 KB of the file is upstream wording.

`data/endpoints.json` was produced by analysing the same package to recover which
GitLab API endpoint serves each tool. The endpoint facts themselves describe
GitLab's public API; the `notes` fields are this project's own.

Everything under `src/` is original work.

Upstream licence, reproduced as MIT requires:

```
MIT License

Copyright (c) 2025 Roo

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

This project is not affiliated with GitLab Inc. or with the upstream authors.
