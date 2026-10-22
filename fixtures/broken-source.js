/**
 * @name Broken Syntax Fixture
 * @description Deliberately invalid JavaScript
 * @version 1.0.0
 * @author LX GPUI Player contributors
 * @homepage https://example.invalid/lx-gpui-player
 */
// SPDX-License-Identifier: CC0-1.0
// Intentional parse error: import must be inert; enable must report failure.
lx.on(lx.EVENT_NAMES.request, ( => Promise.resolve("unreachable"));
