# Vendored libraries

The app uses two tiny libraries, vendored unmodified as ES modules so that
building Iris needs no Node.js toolchain. Only the import specifier in
`hooks.js` was changed (`"preact"` to `"./preact.js"`) and source map
comments were removed.

| File | Package | Version | License |
|---|---|---|---|
| `preact.js` | [preact](https://github.com/preactjs/preact) `dist/preact.mjs` | 11.0.1 | MIT |
| `hooks.js` | [preact](https://github.com/preactjs/preact) `hooks/dist/hooks.mjs` | 11.0.1 | MIT |
| `htm.js` | [htm](https://github.com/developit/htm) `dist/htm.module.js` | 3.1.1 | Apache-2.0 |

To update, download the packages from npm (`npm pack preact htm`) and copy
the same files, repeating the edits above.

## preact (MIT)

The MIT License (MIT)

Copyright (c) 2015-present Jason Miller

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

## htm (Apache-2.0)

Copyright 2018 Google Inc. All Rights Reserved.

Licensed under the Apache License, Version 2.0 (the "License"); you may not
use this file except in compliance with the License. You may obtain a copy of
the License at http://www.apache.org/licenses/LICENSE-2.0 (also in
`LICENSE-APACHE` at the root of this repository).

Unless required by applicable law or agreed to in writing, software
distributed under the License is distributed on an "AS IS" BASIS, WITHOUT
WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied. See the
License for the specific language governing permissions and limitations under
the License.
