/**
 * @name Missing Handler Fixture
 * @description Declares musicUrl without a handler
 * @version 1.0.0
 * @author LX GPUI Player contributors
 * @homepage https://example.invalid/lx-gpui-player
 */
// SPDX-License-Identifier: CC0-1.0
lx.send(lx.EVENT_NAMES.inited, {
  sources: {
    local: {
      name: "Missing Handler Fixture", type: "music",
      actions: ["musicUrl"], qualitys: []
    }
  }
});
