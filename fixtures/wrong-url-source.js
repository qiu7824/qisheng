/**
 * @name Invalid URL Fixture
 * @description Deliberately returns a file URL
 * @version 1.0.0
 * @author LX GPUI Player contributors
 * @homepage https://example.invalid/lx-gpui-player
 */
// SPDX-License-Identifier: CC0-1.0
// This fixture makes no network or filesystem access.
lx.on(lx.EVENT_NAMES.request, () => Promise.resolve("file:///forbidden.wav"));
lx.send(lx.EVENT_NAMES.inited, {
  sources: {
    local: {
      name: "Invalid URL Fixture", type: "music",
      actions: ["musicUrl"], qualitys: []
    }
  }
});
