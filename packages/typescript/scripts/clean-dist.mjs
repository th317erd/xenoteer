#!/usr/bin/env node
// SPDX-License-Identifier: Apache-2.0

import { rmSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const packageRoot = join(dirname(fileURLToPath(import.meta.url)), "..");
rmSync(join(packageRoot, "dist"), { force: true, recursive: true });
